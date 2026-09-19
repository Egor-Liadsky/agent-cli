//! Инварианты: архитектурные решения, ограничения по стеку и бизнес-правила,
//! которые агент обязан соблюдать в любом ответе независимо от того, о чём
//! просит пользователь.
//!
//! Набор инвариантов живёт отдельно от истории чата и от `ChatSettings`:
//! загружается один раз из файла конфигурации при старте процесса и не
//! редактируется диалогом (design.md, «Отдельный тип `InvariantSet`»).

use crate::agent::{Agent, AgentReply, Message};
use crate::pipeline::{OutputPolicy, PolicyOutcome, RequestContext};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Код отказа `InvariantGuard`. Отличается от кодов, которыми входные
/// политики отклоняют запрос (spec.md, «Причина отказа не путается с
/// другими отказами»).
pub const INVARIANT_VIOLATION_CODE: &str = "invariant_violation";

/// Один инвариант: свободная формулировка правила, которое ответ не должен
/// нарушать, плюс необязательное обоснование для человека, читающего
/// конфигурацию.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invariant {
    pub id: String,
    pub statement: String,
    pub category: String,
    #[serde(default)]
    pub rationale: Option<String>,
}

/// Активный набор инвариантов. Пустой набор — обычный режим без проверки
/// (spec.md, «Пустой источник»).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InvariantSet {
    /// `rename` — поле называется как секция `[[invariant]]` в TOML-файле,
    /// а не как это поле в Rust.
    #[serde(default, rename = "invariant")]
    pub invariants: Vec<Invariant>,
}

impl InvariantSet {
    pub fn is_empty(&self) -> bool {
        self.invariants.is_empty()
    }

    pub fn find(&self, id: &str) -> Option<&Invariant> {
        self.invariants.iter().find(|inv| inv.id == id)
    }

    /// Текстовый системный блок со всеми активными инвариантами: отдельная
    /// строка на инвариант с id и категорией, чтобы отказ мог сослаться на
    /// конкретную запись.
    pub fn render(&self) -> String {
        let mut lines =
            vec!["Действующие инварианты (не могут быть изменены диалогом):".to_string()];
        for inv in &self.invariants {
            let mut line = format!("- [{}] ({}) {}", inv.id, inv.category, inv.statement);
            if let Some(rationale) = &inv.rationale {
                line.push_str(&format!(" — {rationale}"));
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    /// Разбор содержимого `invariants.toml`.
    pub fn from_toml_str(content: &str) -> Result<Self> {
        toml::from_str(content).context("не удалось разобрать invariants.toml")
    }

    /// Загрузка из файла на диске. Отсутствующий файл — пустой набор, а не
    /// ошибка: инварианты не обязательны для работы (в отличие от, например,
    /// `AGENTD_UPSTREAM_API_KEY`).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))?;
        Self::from_toml_str(&content)
            .with_context(|| format!("не удалось разобрать {}", path.display()))
    }
}

/// Структурированный вердикт служебного вызова модели в `InvariantGuard`.
#[derive(Debug, Clone, Deserialize)]
struct InvariantVerdict {
    violated: bool,
    #[serde(default)]
    invariant_id: Option<String>,
    #[serde(default)]
    explanation: Option<String>,
}

/// Выходная политика: отклоняет ответ, явно нарушающий один из активных
/// инвариантов. Формулировки инвариантов — свободный текст, поэтому проверка
/// идёт отдельным служебным вызовом к тому же `Agent`, что и основной вызов
/// (design.md, «Проверка через служебный вызов модели, а не через строгие
/// regex-правила»), а не сопоставлением по шаблону.
pub struct InvariantGuard {
    agent: Arc<dyn Agent>,
}

impl InvariantGuard {
    pub fn new(agent: Arc<dyn Agent>) -> Self {
        Self { agent }
    }

    fn prompt(invariants: &InvariantSet, reply: &str) -> Vec<Message> {
        let mut text = String::from(
            "Проверь, нарушает ли ОТВЕТ МОДЕЛИ ниже хотя бы один из перечисленных \
             инвариантов. Инвариант нарушен, только если ответ явно предлагает или \
             описывает его нарушение — не из-за одной лишь темы запроса. Ответь \
             единственным JSON-объектом без пояснений вокруг: {\"violated\": bool, \
             \"invariant_id\": string|null, \"explanation\": string|null}.\n\n",
        );
        text.push_str(&invariants.render());
        text.push_str("\n\nОтвет модели:\n");
        text.push_str(reply);
        vec![Message::user(text)]
    }

    /// Модель иногда оборачивает JSON в ```-блок несмотря на инструкцию —
    /// разбор терпим к этому, а не требует байт-в-байт точный формат.
    fn parse_verdict(content: &str) -> Result<InvariantVerdict> {
        let trimmed = content.trim();
        let json = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .map(|rest| rest.trim_end_matches("```").trim())
            .unwrap_or(trimmed);
        serde_json::from_str(json)
            .with_context(|| format!("вердикт InvariantGuard не в ожидаемом формате JSON: {content}"))
    }
}

#[async_trait]
impl OutputPolicy for InvariantGuard {
    fn name(&self) -> &str {
        "invariant-guard"
    }

    async fn check(&self, context: &RequestContext, reply: &AgentReply) -> Result<PolicyOutcome> {
        if context.invariants.is_empty() {
            return Ok(PolicyOutcome::Pass);
        }

        let history = Self::prompt(&context.invariants, &reply.content);
        let verdict_reply = self.agent.ask(&history, &context.settings).await?;
        let verdict = Self::parse_verdict(&verdict_reply.content)?;

        if !verdict.violated {
            return Ok(PolicyOutcome::Pass);
        }

        let invariant = verdict
            .invariant_id
            .as_deref()
            .and_then(|id| context.invariants.find(id));

        let reason = match invariant {
            Some(inv) => format!(
                "нарушен инвариант `{}`: {}{}",
                inv.id,
                inv.statement,
                verdict
                    .explanation
                    .as_ref()
                    .map(|e| format!(". {e}"))
                    .unwrap_or_default()
            ),
            None => verdict
                .explanation
                .unwrap_or_else(|| "ответ нарушает один из активных инвариантов".to_string()),
        };

        Ok(PolicyOutcome::reject(INVARIANT_VIOLATION_CODE, reason))
    }
}

/// Список выходных политик по умолчанию для потребителей `agentcore`:
/// сейчас единственная запись — `InvariantGuard`, работающая с тем же
/// `Agent`, что и основной вызов конвейера. Регистрация не ломает набор
/// политик при пустом `InvariantSet`: `InvariantGuard::check` тогда просто
/// пропускает ответ без служебного вызова.
pub fn default_output_policies(agent: Arc<dyn Agent>) -> Vec<Arc<dyn OutputPolicy>> {
    vec![Arc::new(InvariantGuard::new(agent))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChatSettings;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn secret_invariant() -> Invariant {
        Invariant {
            id: "no-client-side-secrets".to_string(),
            statement: "ключ провайдера принадлежит сервису, не клиенту".to_string(),
            category: "security".to_string(),
            rationale: Some("клиент не должен хранить ключ".to_string()),
        }
    }

    #[test]
    fn render_lists_every_invariant() {
        let set = InvariantSet {
            invariants: vec![secret_invariant()],
        };
        let text = set.render();
        assert!(text.contains("no-client-side-secrets"));
        assert!(text.contains("security"));
        assert!(text.contains("ключ провайдера"));
    }

    #[test]
    fn empty_set_renders_header_only() {
        let set = InvariantSet::default();
        assert!(set.is_empty());
        assert_eq!(
            set.render(),
            "Действующие инварианты (не могут быть изменены диалогом):"
        );
    }

    #[test]
    fn from_toml_str_parses_invariant_array() {
        let content = r#"
[[invariant]]
id = "no-client-side-secrets"
statement = "ключ провайдера принадлежит сервису"
category = "security"
"#;
        let set = InvariantSet::from_toml_str(content).expect("разбор");
        assert_eq!(set.invariants.len(), 1);
        assert_eq!(set.invariants[0].id, "no-client-side-secrets");
        assert_eq!(set.invariants[0].rationale, None);
    }

    #[test]
    fn from_toml_str_rejects_malformed_content() {
        let err = InvariantSet::from_toml_str("this is not valid toml [[[").unwrap_err();
        assert!(err.to_string().contains("invariants.toml"));
    }

    #[test]
    fn load_missing_file_returns_empty_set() {
        let path = Path::new("/nonexistent/invariants-does-not-exist.toml");
        let set = InvariantSet::load(path).expect("отсутствующий файл — не ошибка");
        assert!(set.is_empty());
    }

    #[test]
    fn load_malformed_file_returns_error() {
        let dir = std::env::temp_dir().join(format!("invariants-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("временная директория");
        let path = dir.join("invariants.toml");
        std::fs::write(&path, "not valid toml [[[").expect("запись файла");
        let err = InvariantSet::load(&path).unwrap_err();
        assert!(err.to_string().contains("invariants.toml"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Агент без сети: считает вызовы и отвечает заранее заданным текстом,
    /// как в тестах `pipeline.rs`.
    struct FakeAgent {
        reply: String,
        calls: AtomicUsize,
    }

    impl FakeAgent {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Agent for FakeAgent {
        async fn ask(&self, _history: &[Message], _settings: &ChatSettings) -> Result<AgentReply> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AgentReply {
                content: self.reply.clone(),
                reasoning: None,
                meta: Default::default(),
                model: None,
                policy: None,
                context: None,
            })
        }
    }

    fn context_with(invariants: InvariantSet) -> RequestContext {
        RequestContext::new("req-1", vec![Message::user("вопрос")], ChatSettings::default())
            .with_invariants(invariants)
    }

    fn reply(content: &str) -> AgentReply {
        AgentReply {
            content: content.to_string(),
            reasoning: None,
            meta: Default::default(),
            model: None,
            policy: None,
            context: None,
        }
    }

    #[tokio::test]
    async fn empty_set_skips_the_service_call_entirely() {
        let agent = FakeAgent::new("не важно");
        let guard = InvariantGuard::new(agent.clone());
        let outcome = guard
            .check(&context_with(InvariantSet::default()), &reply("любой ответ"))
            .await
            .expect("проверка");
        assert!(matches!(outcome, PolicyOutcome::Pass));
        assert_eq!(agent.calls(), 0);
    }

    #[tokio::test]
    async fn violation_verdict_rejects_with_invariant_named() {
        let agent = FakeAgent::new(
            r#"{"violated": true, "invariant_id": "no-client-side-secrets", "explanation": "предложил положить ключ в тело запроса"}"#,
        );
        let guard = InvariantGuard::new(agent.clone());
        let set = InvariantSet {
            invariants: vec![secret_invariant()],
        };
        let outcome = guard
            .check(&context_with(set), &reply("положи ключ в тело запроса"))
            .await
            .expect("проверка");
        match outcome {
            PolicyOutcome::Reject { code, reason } => {
                assert_eq!(code, INVARIANT_VIOLATION_CODE);
                assert!(reason.contains("no-client-side-secrets"));
                assert!(reason.contains("предложил положить ключ"));
            }
            other => panic!("ожидался отказ, получено {other:?}"),
        }
        assert_eq!(agent.calls(), 1);
    }

    #[tokio::test]
    async fn no_violation_verdict_passes() {
        let agent = FakeAgent::new(r#"{"violated": false, "invariant_id": null, "explanation": null}"#);
        let guard = InvariantGuard::new(agent);
        let set = InvariantSet {
            invariants: vec![secret_invariant()],
        };
        let outcome = guard
            .check(&context_with(set), &reply("обычный ответ"))
            .await
            .expect("проверка");
        assert!(matches!(outcome, PolicyOutcome::Pass));
    }

    #[tokio::test]
    async fn invariant_violation_code_differs_from_input_policy_codes() {
        assert_ne!(INVARIANT_VIOLATION_CODE, "blocked");
        assert_ne!(INVARIANT_VIOLATION_CODE, "unsafe");
    }

    #[tokio::test]
    async fn pipeline_places_violation_in_output_log_not_input() {
        use crate::pipeline::{Pipeline, PipelineOutcome};

        let agent = FakeAgent::new(
            r#"{"violated": true, "invariant_id": "no-client-side-secrets", "explanation": "предложил вынести ключ в тело запроса"}"#,
        );
        let policies = default_output_policies(agent.clone());
        let pipeline = Pipeline::new(agent.clone()).with_output_policies(policies);
        let context = context_with(InvariantSet {
            invariants: vec![secret_invariant()],
        });

        match pipeline.run(context).await.expect("конвейер") {
            PipelineOutcome::Rejected {
                stage,
                code,
                policy,
                ..
            } => {
                assert_eq!(stage, "invariant-guard");
                assert_eq!(code, INVARIANT_VIOLATION_CODE);
                assert!(policy.input.is_empty(), "отказ не должен попасть во входной лог");
                assert_eq!(policy.output.len(), 1);
                assert_eq!(policy.output[0].stage, "invariant-guard");
            }
            PipelineOutcome::Completed { .. } => panic!("ожидался отказ"),
        }
    }

    #[tokio::test]
    async fn default_output_policies_do_not_break_empty_invariant_set() {
        use crate::pipeline::{AllowAllInput, Pipeline, PipelineOutcome};

        let agent = FakeAgent::new("обычный ответ");
        let policies = default_output_policies(agent.clone());
        let pipeline = Pipeline::new(agent.clone())
            .with_input_policies(vec![Arc::new(AllowAllInput)])
            .with_output_policies(policies);
        let context = context_with(InvariantSet::default());

        match pipeline.run(context).await.expect("конвейер") {
            PipelineOutcome::Completed { reply, .. } => {
                assert_eq!(reply.content, "обычный ответ");
            }
            PipelineOutcome::Rejected { stage, code, .. } => {
                panic!("не ожидался отказ, получен {stage}/{code}")
            }
        }
        assert_eq!(agent.calls(), 1, "InvariantGuard не должен вызывать агента при пустом наборе");
    }
}
