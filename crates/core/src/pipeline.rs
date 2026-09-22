//! Явный конвейер обработки запроса к агенту.
//!
//! Порядок стадий фиксирован: нормализация запроса, входные политики, вызов
//! модели, выходные политики, судья. Стадии передаются конвейеру списками,
//! поэтому новое правило добавляется регистрацией реализации, а не правкой
//! конвейера или обработчика транспортного уровня.

use crate::agent::{Agent, AgentReply, Message, Role, ToolSpec};
use crate::config::ChatSettings;
use crate::invariants::InvariantSet;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Что стадия сделала с запросом или ответом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyAction {
    /// Пропустила без изменений.
    Pass,
    /// Изменила полезную нагрузку.
    Rewrite,
    /// Отклонила с причиной и кодом.
    Reject,
}

/// След выполненной стадии: попадает в журнал конвейера и в ответ клиенту.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    /// Имя стадии, вернувшей результат.
    pub stage: String,
    pub action: PolicyAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PolicyRecord {
    pub fn pass(stage: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            action: PolicyAction::Pass,
            code: None,
            reason: None,
        }
    }

    pub fn rewrite(stage: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            action: PolicyAction::Rewrite,
            code: None,
            reason: None,
        }
    }

    pub fn reject(stage: impl Into<String>, code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            action: PolicyAction::Reject,
            code: Some(code.into()),
            reason: Some(reason.into()),
        }
    }
}

/// Вердикт судьи по ответу модели.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    pub stage: String,
    /// Итог оценки в терминах самого судьи: например `ok` или `revise`.
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Результаты уже выполненных стадий. Всегда присутствует, даже когда пуст.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyLog {
    #[serde(default)]
    pub input: Vec<PolicyRecord>,
    #[serde(default)]
    pub output: Vec<PolicyRecord>,
    #[serde(default)]
    pub judge: Option<JudgeVerdict>,
}

impl PolicyLog {
    pub fn is_empty(&self) -> bool {
        self.input.is_empty() && self.output.is_empty() && self.judge.is_none()
    }
}

/// Общий для всех стадий контекст одного запроса.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Один и тот же на протяжении всей обработки.
    pub request_id: String,
    pub history: Vec<Message>,
    pub settings: ChatSettings,
    /// Кто прислал запрос, если транспорт это знает.
    pub client_id: Option<String>,
    pub policy: PolicyLog,
    /// Активные инварианты этого процесса: не приходят с запросом и не
    /// сериализуются в файл чата (design.md, «Отдельный тип `InvariantSet`»).
    pub invariants: InvariantSet,
    /// Инструменты, доступные модели на этом вызове. Принадлежат запуску
    /// клиента, а не чату, поэтому живут в контексте, а не в `settings`.
    pub tools: Vec<ToolSpec>,
}

impl RequestContext {
    pub fn new(request_id: impl Into<String>, history: Vec<Message>, settings: ChatSettings) -> Self {
        Self {
            request_id: request_id.into(),
            history,
            settings,
            client_id: None,
            policy: PolicyLog::default(),
            invariants: InvariantSet::default(),
            tools: Vec::new(),
        }
    }

    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    pub fn with_invariants(mut self, invariants: InvariantSet) -> Self {
        self.invariants = invariants;
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }
}

/// Изменённая стадией полезная нагрузка. Изменение возвращается отдельным
/// значением, а не правкой контекста на месте: так оно попадает в журнал
/// стадий и не даёт политике незаметно поменять чужие поля контекста.
#[derive(Debug, Clone)]
pub enum PolicyPayload {
    /// Изменённая история сообщений (исход входной политики).
    History(Vec<Message>),
    /// Изменённый ответ модели (исход выходной политики). В `Box`, чтобы не
    /// раздувать размер `PolicyOutcome` до размера самого большого варианта
    /// на каждый вызов, включая частый `Pass`.
    Reply(Box<AgentReply>),
}

/// Один из трёх исходов политики.
#[derive(Debug, Clone)]
pub enum PolicyOutcome {
    Pass,
    Rewrite(Box<PolicyPayload>),
    Reject { code: String, reason: String },
}

impl PolicyOutcome {
    pub fn reject(code: impl Into<String>, reason: impl Into<String>) -> Self {
        PolicyOutcome::Reject {
            code: code.into(),
            reason: reason.into(),
        }
    }
}

/// Итог всего конвейера. Отказ правила — это `Rejected`, а внутренняя ошибка
/// стадии — `Err` от [`Pipeline::run`]: транспорт отображает их разными кодами.
#[derive(Debug, Clone)]
pub enum PipelineOutcome {
    Completed {
        /// В `Box` по той же причине, что и `PolicyPayload::Reply`: не
        /// раздувать размер варианта `Rejected`, которого этот вариант не
        /// разделяет.
        reply: Box<AgentReply>,
        policy: PolicyLog,
    },
    Rejected {
        stage: String,
        code: String,
        reason: String,
        policy: PolicyLog,
    },
}

#[async_trait]
pub trait InputPolicy: Send + Sync {
    fn name(&self) -> &str;

    /// `Err` означает, что стадия не смогла выполниться, а не что правило
    /// сработало: для отказа есть [`PolicyOutcome::Reject`].
    async fn check(&self, context: &RequestContext) -> Result<PolicyOutcome>;
}

#[async_trait]
pub trait OutputPolicy: Send + Sync {
    fn name(&self) -> &str;

    async fn check(&self, context: &RequestContext, reply: &AgentReply) -> Result<PolicyOutcome>;
}

#[async_trait]
pub trait Judge: Send + Sync {
    fn name(&self) -> &str;

    /// Судья ходит к модели через тот же интерфейс агента, что и основной
    /// вызов, и получает его экземпляр от конвейера.
    async fn review(
        &self,
        context: &RequestContext,
        reply: &AgentReply,
        agent: &dyn Agent,
    ) -> Result<JudgeVerdict>;
}

/// Входная политика, пропускающая всё.
pub struct AllowAllInput;

#[async_trait]
impl InputPolicy for AllowAllInput {
    fn name(&self) -> &str {
        "allow-all-input"
    }

    async fn check(&self, _context: &RequestContext) -> Result<PolicyOutcome> {
        Ok(PolicyOutcome::Pass)
    }
}

/// Выходная политика, пропускающая всё.
pub struct AllowAllOutput;

#[async_trait]
impl OutputPolicy for AllowAllOutput {
    fn name(&self) -> &str {
        "allow-all-output"
    }

    async fn check(&self, _context: &RequestContext, _reply: &AgentReply) -> Result<PolicyOutcome> {
        Ok(PolicyOutcome::Pass)
    }
}

/// Судья, который ничего не проверяет и к модели не обращается.
pub struct NoopJudge;

#[async_trait]
impl Judge for NoopJudge {
    fn name(&self) -> &str {
        "noop-judge"
    }

    async fn review(
        &self,
        _context: &RequestContext,
        _reply: &AgentReply,
        _agent: &dyn Agent,
    ) -> Result<JudgeVerdict> {
        Ok(JudgeVerdict {
            stage: self.name().to_string(),
            verdict: "ok".to_string(),
            score: None,
            reason: None,
        })
    }
}

/// Конвейер обработки запроса. Агент приходит извне, конвейер его не создаёт.
pub struct Pipeline {
    agent: Arc<dyn Agent>,
    input: Vec<Arc<dyn InputPolicy>>,
    output: Vec<Arc<dyn OutputPolicy>>,
    judge: Option<Arc<dyn Judge>>,
}

impl Pipeline {
    pub fn new(agent: Arc<dyn Agent>) -> Self {
        Self {
            agent,
            input: Vec::new(),
            output: Vec::new(),
            judge: None,
        }
    }

    pub fn with_input_policies(mut self, policies: Vec<Arc<dyn InputPolicy>>) -> Self {
        self.input = policies;
        self
    }

    pub fn with_output_policies(mut self, policies: Vec<Arc<dyn OutputPolicy>>) -> Self {
        self.output = policies;
        self
    }

    pub fn with_judge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// История, которая уходит модели на основном вызове: при непустом
    /// наборе инвариантов перед сообщениями пользователя добавляется
    /// отдельный системный блок с их текстом (spec.md, «Инварианты явно
    /// присутствуют в контексте рассуждения»). `context.history` при этом не
    /// меняется: более поздние стадии продолжают видеть исходную историю.
    fn history_with_invariants(context: &RequestContext) -> Vec<Message> {
        if context.invariants.is_empty() {
            return context.history.clone();
        }
        let mut history = Vec::with_capacity(context.history.len() + 1);
        history.push(Message::system(context.invariants.render()));
        history.extend(context.history.iter().cloned());
        history
    }

    /// Нормализация запроса: убираются сообщения, состоящие из одних пробелов,
    /// у остальных обрезаются краевые пробелы.
    ///
    /// Ответ модели из одних `tool_calls` и результат инструмента остаются
    /// даже с пустым текстом: без них история хода с инструментами теряет
    /// связь «вызов → результат», которую провайдер требует.
    fn normalize(history: &mut Vec<Message>) {
        history.retain(|message| {
            !message.content.trim().is_empty()
                || !message.tool_calls.is_empty()
                || matches!(message.role, Role::Tool)
        });
        for message in history.iter_mut() {
            let trimmed = message.content.trim();
            if trimmed.len() != message.content.len() {
                message.content = trimmed.to_string();
            }
        }
    }

    pub async fn run(&self, mut context: RequestContext) -> Result<PipelineOutcome> {
        Self::normalize(&mut context.history);

        for policy in &self.input {
            let outcome = policy.check(&context).await?;
            match outcome {
                PolicyOutcome::Pass => context.policy.input.push(PolicyRecord::pass(policy.name())),
                PolicyOutcome::Rewrite(payload) => match *payload {
                    PolicyPayload::History(history) => {
                        context.history = history;
                        Self::normalize(&mut context.history);
                        context
                            .policy
                            .input
                            .push(PolicyRecord::rewrite(policy.name()));
                    }
                    PolicyPayload::Reply(_) => {
                        anyhow::bail!(
                            "входная политика {} вернула изменённый ответ вместо истории",
                            policy.name()
                        );
                    }
                },
                PolicyOutcome::Reject { code, reason } => {
                    context
                        .policy
                        .input
                        .push(PolicyRecord::reject(policy.name(), &code, &reason));
                    return Ok(PipelineOutcome::Rejected {
                        stage: policy.name().to_string(),
                        code,
                        reason,
                        policy: context.policy,
                    });
                }
            }
        }

        let mut reply = self
            .agent
            .ask_with_tools(
                &Self::history_with_invariants(&context),
                &context.settings,
                &context.tools,
            )
            .await?;

        for policy in &self.output {
            let outcome = policy.check(&context, &reply).await?;
            match outcome {
                PolicyOutcome::Pass => context
                    .policy
                    .output
                    .push(PolicyRecord::pass(policy.name())),
                PolicyOutcome::Rewrite(payload) => match *payload {
                    PolicyPayload::Reply(rewritten) => {
                        reply = *rewritten;
                        context
                            .policy
                            .output
                            .push(PolicyRecord::rewrite(policy.name()));
                    }
                    PolicyPayload::History(_) => {
                        anyhow::bail!(
                            "выходная политика {} вернула изменённую историю вместо ответа",
                            policy.name()
                        );
                    }
                },
                PolicyOutcome::Reject { code, reason } => {
                    context
                        .policy
                        .output
                        .push(PolicyRecord::reject(policy.name(), &code, &reason));
                    return Ok(PipelineOutcome::Rejected {
                        stage: policy.name().to_string(),
                        code,
                        reason,
                        policy: context.policy,
                    });
                }
            }
        }

        // Промежуточный ответ (модель ещё собирает данные инструментами)
        // оценивать нечего: судья смотрит только окончательный.
        if let Some(judge) = self.judge.as_ref().filter(|_| reply.tool_calls.is_empty()) {
            let verdict = judge.review(&context, &reply, self.agent.as_ref()).await?;
            context.policy.judge = Some(verdict);
        }

        Ok(PipelineOutcome::Completed {
            reply: Box::new(reply),
            policy: context.policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{MessageMeta, ToolCall};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Агент без сети: считает вызовы и запоминает последнюю историю.
    struct FakeAgent {
        reply: String,
        tool_calls: Vec<ToolCall>,
        calls: AtomicUsize,
        last_history: std::sync::Mutex<Vec<Message>>,
        last_tools: std::sync::Mutex<Vec<ToolSpec>>,
    }

    impl FakeAgent {
        fn new(reply: &str) -> Arc<Self> {
            Self::with_tool_calls(reply, Vec::new())
        }

        fn with_tool_calls(reply: &str, tool_calls: Vec<ToolCall>) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                tool_calls,
                calls: AtomicUsize::new(0),
                last_history: std::sync::Mutex::new(Vec::new()),
                last_tools: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last_history(&self) -> Vec<Message> {
            self.last_history.lock().expect("история").clone()
        }
    }

    #[async_trait]
    impl Agent for FakeAgent {
        async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply> {
            self.ask_with_tools(history, settings, &[]).await
        }

        async fn ask_with_tools(
            &self,
            history: &[Message],
            _settings: &ChatSettings,
            tools: &[ToolSpec],
        ) -> Result<AgentReply> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_history.lock().expect("история") = history.to_vec();
            *self.last_tools.lock().expect("инструменты") = tools.to_vec();
            Ok(AgentReply {
                content: self.reply.clone(),
                reasoning: None,
                meta: MessageMeta::default(),
                model: None,
                policy: None,
                context: None,
                tool_calls: self.tool_calls.clone(),
            })
        }
    }

    fn context() -> RequestContext {
        RequestContext::new(
            "req-1",
            vec![Message::user("исходный вопрос")],
            ChatSettings::default(),
        )
    }

    fn completed(outcome: PipelineOutcome) -> (AgentReply, PolicyLog) {
        match outcome {
            PipelineOutcome::Completed { reply, policy } => (*reply, policy),
            PipelineOutcome::Rejected { stage, code, .. } => {
                panic!("ожидался Completed, получен отказ {stage}/{code}")
            }
        }
    }

    struct RewriteHistory;

    #[async_trait]
    impl InputPolicy for RewriteHistory {
        fn name(&self) -> &str {
            "rewrite-history"
        }

        async fn check(&self, _context: &RequestContext) -> Result<PolicyOutcome> {
            Ok(PolicyOutcome::Rewrite(Box::new(PolicyPayload::History(vec![
                Message::user("изменённый вопрос"),
            ]))))
        }
    }

    struct RejectInput;

    #[async_trait]
    impl InputPolicy for RejectInput {
        fn name(&self) -> &str {
            "reject-input"
        }

        async fn check(&self, _context: &RequestContext) -> Result<PolicyOutcome> {
            Ok(PolicyOutcome::reject("blocked", "запрос запрещён правилом"))
        }
    }

    struct FailingInput;

    #[async_trait]
    impl InputPolicy for FailingInput {
        fn name(&self) -> &str {
            "failing-input"
        }

        async fn check(&self, _context: &RequestContext) -> Result<PolicyOutcome> {
            anyhow::bail!("стадия не смогла выполниться")
        }
    }

    /// Выходная политика, которая видит след входной стадии в контексте.
    struct MaskOutput;

    #[async_trait]
    impl OutputPolicy for MaskOutput {
        fn name(&self) -> &str {
            "mask-output"
        }

        async fn check(&self, context: &RequestContext, reply: &AgentReply) -> Result<PolicyOutcome> {
            assert_eq!(
                context.policy.input.len(),
                1,
                "выходная политика должна видеть результат входной"
            );
            let mut masked = reply.clone();
            masked.content = "***".to_string();
            Ok(PolicyOutcome::Rewrite(Box::new(PolicyPayload::Reply(Box::new(masked)))))
        }
    }

    struct RejectOutput;

    #[async_trait]
    impl OutputPolicy for RejectOutput {
        fn name(&self) -> &str {
            "reject-output"
        }

        async fn check(&self, _context: &RequestContext, _reply: &AgentReply) -> Result<PolicyOutcome> {
            Ok(PolicyOutcome::reject("unsafe", "ответ запрещён правилом"))
        }
    }

    /// Судья, который сам обращается к модели через переданного агента.
    struct AskingJudge;

    #[async_trait]
    impl Judge for AskingJudge {
        fn name(&self) -> &str {
            "asking-judge"
        }

        async fn review(
            &self,
            context: &RequestContext,
            _reply: &AgentReply,
            agent: &dyn Agent,
        ) -> Result<JudgeVerdict> {
            let review = agent
                .ask(&context.history, &context.settings)
                .await?;
            Ok(JudgeVerdict {
                stage: self.name().to_string(),
                verdict: "ok".to_string(),
                score: None,
                reason: Some(review.content),
            })
        }
    }

    #[tokio::test]
    async fn full_pass_returns_completed() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent.clone())
            .with_input_policies(vec![Arc::new(AllowAllInput)])
            .with_output_policies(vec![Arc::new(AllowAllOutput)])
            .with_judge(Arc::new(NoopJudge));

        let (reply, policy) = completed(pipeline.run(context()).await.expect("конвейер"));
        assert_eq!(reply.content, "ответ модели");
        assert_eq!(policy.input.len(), 1);
        assert_eq!(policy.output.len(), 1);
        assert_eq!(policy.judge.expect("вердикт").verdict, "ok");
        assert_eq!(agent.calls(), 1);
    }

    #[tokio::test]
    async fn empty_pipeline_matches_noop_pipeline() {
        let bare_agent = FakeAgent::new("ответ модели");
        let (bare_reply, bare_policy) = completed(
            Pipeline::new(bare_agent)
                .run(context())
                .await
                .expect("конвейер"),
        );
        assert!(bare_policy.is_empty());
        assert!(bare_policy.judge.is_none());

        let noop_agent = FakeAgent::new("ответ модели");
        let (noop_reply, _) = completed(
            Pipeline::new(noop_agent)
                .with_input_policies(vec![Arc::new(AllowAllInput)])
                .with_output_policies(vec![Arc::new(AllowAllOutput)])
                .run(context())
                .await
                .expect("конвейер"),
        );
        assert_eq!(bare_reply.content, noop_reply.content);
    }

    #[tokio::test]
    async fn pass_keeps_history_unchanged() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent.clone()).with_input_policies(vec![Arc::new(AllowAllInput)]);
        completed(pipeline.run(context()).await.expect("конвейер"));
        assert_eq!(agent.last_history()[0].content, "исходный вопрос");
    }

    #[tokio::test]
    async fn input_rewrite_reaches_model() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent.clone()).with_input_policies(vec![Arc::new(RewriteHistory)]);
        let (_, policy) = completed(pipeline.run(context()).await.expect("конвейер"));
        assert_eq!(agent.last_history()[0].content, "изменённый вопрос");
        assert_eq!(policy.input[0].action, PolicyAction::Rewrite);
    }

    #[tokio::test]
    async fn output_rewrite_changes_reply() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent)
            .with_input_policies(vec![Arc::new(AllowAllInput)])
            .with_output_policies(vec![Arc::new(MaskOutput)]);
        let (reply, policy) = completed(pipeline.run(context()).await.expect("конвейер"));
        assert_eq!(reply.content, "***");
        assert_eq!(policy.output[0].action, PolicyAction::Rewrite);
    }

    #[tokio::test]
    async fn input_reject_skips_model() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent.clone()).with_input_policies(vec![Arc::new(RejectInput)]);
        match pipeline.run(context()).await.expect("конвейер") {
            PipelineOutcome::Rejected {
                stage,
                code,
                reason,
                policy,
            } => {
                assert_eq!(stage, "reject-input");
                assert_eq!(code, "blocked");
                assert_eq!(reason, "запрос запрещён правилом");
                assert_eq!(policy.input[0].action, PolicyAction::Reject);
            }
            PipelineOutcome::Completed { .. } => panic!("ожидался отказ"),
        }
        assert_eq!(agent.calls(), 0);
    }

    #[tokio::test]
    async fn output_reject_hides_reply() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent.clone()).with_output_policies(vec![Arc::new(RejectOutput)]);
        match pipeline.run(context()).await.expect("конвейер") {
            PipelineOutcome::Rejected { stage, code, .. } => {
                assert_eq!(stage, "reject-output");
                assert_eq!(code, "unsafe");
            }
            PipelineOutcome::Completed { .. } => panic!("ожидался отказ"),
        }
        assert_eq!(agent.calls(), 1);
    }

    #[tokio::test]
    async fn stage_error_differs_from_reject() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent.clone()).with_input_policies(vec![Arc::new(FailingInput)]);
        let err = pipeline
            .run(context())
            .await
            .expect_err("ошибка стадии — это Err, а не Rejected");
        assert!(err.to_string().contains("стадия не смогла выполниться"));
        assert_eq!(agent.calls(), 0);
    }

    #[tokio::test]
    async fn judge_uses_the_same_agent() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent.clone()).with_judge(Arc::new(AskingJudge));
        let (_, policy) = completed(pipeline.run(context()).await.expect("конвейер"));
        let verdict = policy.judge.expect("вердикт");
        assert_eq!(verdict.stage, "asking-judge");
        assert_eq!(verdict.reason.as_deref(), Some("ответ модели"));
        assert_eq!(agent.calls(), 2, "основной вызов и вызов судьи");
    }

    #[tokio::test]
    async fn no_judge_leaves_verdict_empty() {
        let agent = FakeAgent::new("ответ модели");
        let (_, policy) = completed(
            Pipeline::new(agent.clone())
                .run(context())
                .await
                .expect("конвейер"),
        );
        assert!(policy.judge.is_none());
        assert_eq!(agent.calls(), 1);
    }

    #[tokio::test]
    async fn invariants_block_reaches_model_as_separate_system_message() {
        use crate::invariants::{Invariant, InvariantSet};

        let agent = FakeAgent::new("ответ модели");
        let set = InvariantSet {
            invariants: vec![Invariant {
                id: "no-client-side-secrets".to_string(),
                statement: "ключ провайдера принадлежит сервису".to_string(),
                category: "security".to_string(),
                rationale: None,
            }],
        };
        let context = context().with_invariants(set);
        let pipeline = Pipeline::new(agent.clone());
        completed(pipeline.run(context).await.expect("конвейер"));

        let history = agent.last_history();
        assert_eq!(history.len(), 2, "блок инвариантов плюс исходный вопрос");
        assert!(matches!(history[0].role, crate::agent::Role::System));
        assert!(history[0].content.contains("no-client-side-secrets"));
        assert_eq!(history[1].content, "исходный вопрос");
    }

    #[tokio::test]
    async fn empty_invariant_set_matches_behavior_before_the_change() {
        let agent = FakeAgent::new("ответ модели");
        let context = context().with_invariants(crate::invariants::InvariantSet::default());
        let pipeline = Pipeline::new(agent.clone());
        completed(pipeline.run(context).await.expect("конвейер"));

        let history = agent.last_history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "исходный вопрос");
    }

    #[tokio::test]
    async fn extra_policy_runs_in_registration_order() {
        let agent = FakeAgent::new("ответ модели");
        let pipeline = Pipeline::new(agent)
            .with_input_policies(vec![Arc::new(AllowAllInput), Arc::new(RewriteHistory)]);
        let (_, policy) = completed(pipeline.run(context()).await.expect("конвейер"));
        assert_eq!(policy.input[0].stage, "allow-all-input");
        assert_eq!(policy.input[1].stage, "rewrite-history");
    }

    fn git_status_call() -> ToolCall {
        ToolCall {
            id: "call_0".into(),
            name: "git_status".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn tool_calls_only_messages_survive_normalize() {
        let agent = FakeAgent::new("ответ модели");
        let context = RequestContext::new(
            "req-1",
            vec![
                Message::user("статус?"),
                Message::assistant_with_tool_calls("  ", vec![git_status_call()]),
                Message::tool_result("call_0", "git_status", ""),
            ],
            ChatSettings::default(),
        );
        completed(Pipeline::new(agent.clone()).run(context).await.expect("конвейер"));
        let history = agent.last_history();
        assert_eq!(history.len(), 3);
        assert_eq!(history[1].tool_calls.len(), 1);
        assert!(matches!(history[2].role, Role::Tool));
    }

    #[tokio::test]
    async fn judge_skips_intermediate_tool_call_reply() {
        let agent = FakeAgent::with_tool_calls("", vec![git_status_call()]);
        let pipeline = Pipeline::new(agent.clone()).with_judge(Arc::new(AskingJudge));
        let (reply, policy) = completed(pipeline.run(context()).await.expect("конвейер"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert!(policy.judge.is_none());
        assert_eq!(agent.calls(), 1, "судья к модели не ходил");
    }

    #[tokio::test]
    async fn request_tools_reach_agent() {
        let agent = FakeAgent::new("ответ модели");
        let spec = ToolSpec {
            name: "git_status".into(),
            description: None,
            parameters: serde_json::json!({ "type": "object" }),
        };
        let context = context().with_tools(vec![spec.clone()]);
        completed(Pipeline::new(agent.clone()).run(context).await.expect("конвейер"));
        assert_eq!(*agent.last_tools.lock().unwrap(), vec![spec]);
    }
}
