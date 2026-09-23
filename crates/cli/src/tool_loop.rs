//! Цикл инструментов одного хода: запрос к модели с описаниями
//! инструментов, выполнение вызовов, результаты обратно модели — до
//! окончательного ответа или лимита итераций.
//!
//! Цикл живёт в клиенте, а не в ядре и не на сервисе: выполнять инструменты
//! может только тот, кто видит файловую систему пользователя и может
//! спросить у него подтверждение. Модель, исполнитель и подтверждение
//! приходят трейтами, поэтому цикл тестируется без процесса и сети.

use agentclient::ServerAgent;
use agentcore::agent::{Agent, AgentError, AgentReply, Message, ToolCall, ToolSpec};
use agentcore::config::ChatSettings;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;

/// Не больше стольких вызовов выполняется за одну итерацию: модель,
/// выдавшая сотню вызовов разом, скорее ошиблась, чем действительно их
/// хочет.
pub const MAX_CALLS_PER_ITERATION: usize = 16;

pub const NOT_ALLOWED_RESULT: &str = "инструмент не разрешён в настройках чата: вызов не выполнен";
pub const REJECTED_RESULT: &str = "пользователь отклонил вызов";
pub const LIMIT_RESULT: &str =
    "лимит вызовов инструментов исчерпан, ответь по уже полученным данным";
pub const TOO_MANY_CALLS_RESULT: &str =
    "слишком много вызовов за один ответ: этот вызов не выполнен, повтори его следующим шагом";

/// Исполнитель инструментов (для git — процесс `git-mcp`).
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Описания инструментов, доступных модели в этом ходе.
    fn specs(&self) -> Vec<ToolSpec>;
    /// Пишущий инструмент требует подтверждения человека.
    fn is_write(&self, name: &str) -> bool;
    /// Текст результата для модели. `Err` с [`AgentError::ToolServerUnavailable`]
    /// прерывает ход; прочие ошибки становятся результатом-ошибкой.
    async fn call(&self, call: &ToolCall) -> Result<String>;
}

/// Подтверждение пишущего вызова.
#[async_trait]
pub trait ToolApprover: Send + Sync {
    async fn approve(&self, call: &ToolCall) -> bool;

    /// Результат для модели, если вызов не подтверждён.
    fn refusal(&self) -> &str {
        REJECTED_RESULT
    }
}

/// Откуда ход получает ответы модели. Облачный чат ведёт историю на
/// сервисе и шлёт только новое; локальный — собирает историю сам.
#[async_trait]
pub trait TurnBackend: Send {
    /// Первый запрос хода.
    async fn first(&mut self, tools: &[ToolSpec]) -> Result<AgentReply>;
    /// Следующий запрос: `turn` — все сообщения хода после реплики
    /// пользователя (ответы с вызовами и результаты), последними идут
    /// результаты только что выполненных вызовов.
    async fn next(&mut self, turn: &[Message], tools: &[ToolSpec]) -> Result<AgentReply>;
}

/// Что цикл сообщает интерфейсу по ходу работы.
pub trait TurnObserver: Send + Sync {
    /// Новые промежуточные сообщения хода (ответ с вызовами, результаты).
    fn messages(&self, _messages: &[Message]) {}
    /// Сейчас выполняется этот вызов.
    fn running(&self, _call: &ToolCall) {}
}

/// Ход прервался ошибкой. Промежуточные сообщения интерфейс уже получил
/// через наблюдателя; `executed` говорит, произошли ли побочные эффекты:
/// если хоть один вызов выполнен, локальный чат обязан записать
/// выполненную часть.
#[derive(Debug)]
pub struct TurnError {
    pub error: anyhow::Error,
    /// Сколько вызовов дошло до исполнителя.
    pub executed: usize,
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

/// Ответ модели с вызовами как сообщение истории.
fn assistant_message(reply: &AgentReply) -> Message {
    let mut message = Message::assistant_with_tool_calls(reply.content.clone(), reply.tool_calls.clone());
    message.reasoning = reply.reasoning.clone();
    message.meta = Some(reply.meta.clone());
    message
}

fn is_server_unavailable(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<AgentError>(),
        Some(AgentError::ToolServerUnavailable { .. })
    )
}

/// Ход с инструментами.
///
/// Итерация — один ответ модели с непустым `tool_calls`. Вызовы
/// выполняются последовательно, в порядке ответа: сервер работает с одной
/// рабочей копией, а порядок `git_add` → `git_commit` важен. После
/// `max_iterations` итераций вызовы следующего ответа не выполняются, и
/// модель получает один финальный запрос без инструментов; если она и тогда
/// зовёт инструменты — [`AgentError::ToolLoopLimit`].
pub async fn run_tool_loop(
    backend: &mut dyn TurnBackend,
    executor: &dyn ToolExecutor,
    approver: &dyn ToolApprover,
    max_iterations: u32,
    observer: &dyn TurnObserver,
) -> std::result::Result<AgentReply, TurnError> {
    let specs = executor.specs();
    let allowed: HashSet<String> = specs.iter().map(|spec| spec.name.clone()).collect();
    let mut messages: Vec<Message> = Vec::new();
    let mut executed = 0usize;
    let mut iterations = 0u32;
    let mut forced_final = false;

    let fail = |error: anyhow::Error, executed: usize| TurnError { error, executed };

    let mut reply = match backend.first(&specs).await {
        Ok(reply) => reply,
        Err(error) => return Err(fail(error, executed)),
    };

    loop {
        if reply.tool_calls.is_empty() {
            return Ok(reply);
        }
        if forced_final {
            let error = AgentError::ToolLoopLimit { iterations }.into();
            return Err(fail(error, executed));
        }
        let assistant = assistant_message(&reply);
        messages.push(assistant.clone());
        observer.messages(std::slice::from_ref(&assistant));

        let exhausted = iterations >= max_iterations;
        if !exhausted {
            iterations += 1;
        }
        let mut results = Vec::with_capacity(reply.tool_calls.len());
        for (index, call) in reply.tool_calls.iter().enumerate() {
            let text = if exhausted {
                LIMIT_RESULT.to_string()
            } else if index >= MAX_CALLS_PER_ITERATION {
                TOO_MANY_CALLS_RESULT.to_string()
            } else if !allowed.contains(&call.name) {
                NOT_ALLOWED_RESULT.to_string()
            } else if executor.is_write(&call.name) && !approver.approve(call).await {
                approver.refusal().to_string()
            } else {
                observer.running(call);
                executed += 1;
                match executor.call(call).await {
                    Ok(text) => text,
                    Err(error) if is_server_unavailable(&error) => {
                        observer.messages(&results);
                        return Err(fail(error, executed));
                    }
                    Err(error) => format!("Ошибка инструмента: {error:#}"),
                }
            };
            results.push(Message::tool_result(call.id.clone(), call.name.clone(), text));
        }
        observer.messages(&results);
        messages.extend(results);

        let tools: &[ToolSpec] = if exhausted {
            forced_final = true;
            &[]
        } else {
            &specs
        };
        reply = match backend.next(&messages, tools).await {
            Ok(reply) => reply,
            Err(error) => return Err(fail(error, executed)),
        };
    }
}

/// Результаты инструментов в конце сообщений хода: то, что облачный чат
/// отправляет сервису продолжением.
fn trailing_results(turn: &[Message]) -> &[Message] {
    let start = turn
        .iter()
        .rposition(|message| !message.tool_calls.is_empty())
        .map(|index| index + 1)
        .unwrap_or(0);
    &turn[start..]
}

/// Облачный чат: историю ведёт сервис, клиент шлёт реплику, а затем только
/// результаты вызовов.
pub struct CloudTurn<'a> {
    pub server: &'a ServerAgent,
    pub chat_id: &'a str,
    pub prompt: &'a str,
    pub settings: &'a ChatSettings,
}

#[async_trait]
impl TurnBackend for CloudTurn<'_> {
    async fn first(&mut self, tools: &[ToolSpec]) -> Result<AgentReply> {
        self.server
            .ask_in_chat(self.chat_id, self.prompt, self.settings, tools)
            .await
    }

    async fn next(&mut self, turn: &[Message], tools: &[ToolSpec]) -> Result<AgentReply> {
        self.server
            .continue_in_chat(self.chat_id, trailing_results(turn), self.settings, tools)
            .await
    }
}

/// История у клиента: локальная модель и `agentcli ask`. Каждый запрос
/// уходит с полной историей — исходной плюс сообщения хода.
pub struct HistoryTurn<'a> {
    pub agent: &'a dyn Agent,
    pub history: &'a [Message],
    pub settings: &'a ChatSettings,
}

#[async_trait]
impl TurnBackend for HistoryTurn<'_> {
    async fn first(&mut self, tools: &[ToolSpec]) -> Result<AgentReply> {
        self.agent.ask_with_tools(self.history, self.settings, tools).await
    }

    async fn next(&mut self, turn: &[Message], tools: &[ToolSpec]) -> Result<AgentReply> {
        let mut history = self.history.to_vec();
        history.extend(turn.iter().cloned());
        self.agent.ask_with_tools(&history, self.settings, tools).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcore::agent::MessageMeta;
    use serde_json::json;
    use std::sync::Mutex;

    /// Наблюдатель, собирающий промежуточные сообщения хода.
    #[derive(Default)]
    struct Collect(Mutex<Vec<Message>>);

    impl TurnObserver for Collect {
        fn messages(&self, messages: &[Message]) {
            self.0.lock().unwrap().extend(messages.iter().cloned());
        }
    }

    impl Collect {
        fn all(&self) -> Vec<Message> {
            self.0.lock().unwrap().clone()
        }
    }

    fn reply(content: &str, calls: Vec<ToolCall>) -> AgentReply {
        AgentReply {
            content: content.to_string(),
            reasoning: None,
            meta: MessageMeta::default(),
            model: None,
            policy: None,
            context: None,
            tool_calls: calls,
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    /// Модель по сценарию: отдаёт заранее заданные ответы и запоминает, с
    /// какими инструментами и каким ходом её спрашивали.
    struct Script {
        replies: Vec<AgentReply>,
        asked_tools: Vec<usize>,
        turns: Vec<Vec<Message>>,
    }

    impl Script {
        fn new(replies: Vec<AgentReply>) -> Self {
            Self {
                replies: replies.into_iter().rev().collect(),
                asked_tools: Vec::new(),
                turns: Vec::new(),
            }
        }

        fn take(&mut self, tools: &[ToolSpec]) -> Result<AgentReply> {
            self.asked_tools.push(tools.len());
            self.replies.pop().ok_or_else(|| anyhow::anyhow!("сценарий закончился"))
        }
    }

    #[async_trait]
    impl TurnBackend for Script {
        async fn first(&mut self, tools: &[ToolSpec]) -> Result<AgentReply> {
            self.take(tools)
        }

        async fn next(&mut self, turn: &[Message], tools: &[ToolSpec]) -> Result<AgentReply> {
            self.turns.push(turn.to_vec());
            self.take(tools)
        }
    }

    struct FakeExecutor {
        calls: Mutex<Vec<String>>,
        fail_with: Option<fn() -> anyhow::Error>,
    }

    impl FakeExecutor {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn called(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ToolExecutor for FakeExecutor {
        fn specs(&self) -> Vec<ToolSpec> {
            ["git_status", "git_add"]
                .into_iter()
                .map(|name| ToolSpec {
                    name: name.into(),
                    description: None,
                    parameters: json!({ "type": "object" }),
                })
                .collect()
        }

        fn is_write(&self, name: &str) -> bool {
            name != "git_status"
        }

        async fn call(&self, call: &ToolCall) -> Result<String> {
            self.calls.lock().unwrap().push(call.name.clone());
            if let Some(fail) = self.fail_with {
                return Err(fail());
            }
            Ok(format!("вывод {}", call.name))
        }
    }

    struct Approve(bool);

    #[async_trait]
    impl ToolApprover for Approve {
        async fn approve(&self, _call: &ToolCall) -> bool {
            self.0
        }
    }

    #[tokio::test]
    async fn final_reply_in_one_iteration() {
        let mut backend = Script::new(vec![reply("ответ", vec![])]);
        let executor = FakeExecutor::new();
        let seen = Collect::default();
        let outcome = run_tool_loop(&mut backend, &executor, &Approve(true), 8, &seen)
            .await
            .expect("ход");
        assert_eq!(outcome.content, "ответ");
        assert!(seen.all().is_empty());
        assert!(executor.called().is_empty());
        assert_eq!(backend.asked_tools, vec![2]);
    }

    #[tokio::test]
    async fn two_tool_iterations_then_answer() {
        let mut backend = Script::new(vec![
            reply("", vec![call("c0", "git_status")]),
            reply("", vec![call("c1", "git_status")]),
            reply("готово", vec![]),
        ]);
        let executor = FakeExecutor::new();
        let seen = Collect::default();
        let outcome = run_tool_loop(&mut backend, &executor, &Approve(true), 8, &seen)
            .await
            .expect("ход");
        assert_eq!(outcome.content, "готово");
        assert_eq!(executor.called(), vec!["git_status", "git_status"]);
        assert_eq!(seen.all().len(), 4, "два ответа с вызовами и два результата");
        let last_turn = backend.turns.last().unwrap();
        assert_eq!(last_turn.last().unwrap().tool_call_id.as_deref(), Some("c1"));
        assert_eq!(last_turn.last().unwrap().content, "вывод git_status");
    }

    #[tokio::test]
    async fn rejected_write_call_is_not_executed() {
        let mut backend = Script::new(vec![
            reply("", vec![call("c0", "git_add")]),
            reply("не стал добавлять", vec![]),
        ]);
        let executor = FakeExecutor::new();
        let seen = Collect::default();
        run_tool_loop(&mut backend, &executor, &Approve(false), 8, &seen)
            .await
            .expect("ход");
        assert!(executor.called().is_empty());
        assert_eq!(seen.all()[1].content, REJECTED_RESULT);
    }

    #[tokio::test]
    async fn not_allowed_tool_is_not_executed() {
        let mut backend = Script::new(vec![
            reply("", vec![call("c0", "git_commit")]),
            reply("ок", vec![]),
        ]);
        let executor = FakeExecutor::new();
        let seen = Collect::default();
        run_tool_loop(&mut backend, &executor, &Approve(true), 8, &seen)
            .await
            .expect("ход");
        assert!(executor.called().is_empty());
        assert_eq!(seen.all()[1].content, NOT_ALLOWED_RESULT);
    }

    #[tokio::test]
    async fn limit_leads_to_final_request_without_tools() {
        let mut backend = Script::new(vec![
            reply("", vec![call("c0", "git_status")]),
            reply("", vec![call("c1", "git_status")]),
            reply("", vec![call("c2", "git_status")]),
            reply("по имеющимся данным", vec![]),
        ]);
        let executor = FakeExecutor::new();
        let seen = Collect::default();
        let outcome = run_tool_loop(&mut backend, &executor, &Approve(true), 2, &seen)
            .await
            .expect("ход");
        assert_eq!(outcome.content, "по имеющимся данным");
        assert_eq!(executor.called().len(), 2, "третий ответ сверх лимита не выполняется");
        assert_eq!(backend.asked_tools, vec![2, 2, 2, 0], "финальный запрос без инструментов");
        assert_eq!(seen.all().last().unwrap().content, LIMIT_RESULT);
    }

    #[tokio::test]
    async fn calls_after_limit_end_with_loop_limit_error() {
        let mut backend = Script::new(vec![
            reply("", vec![call("c0", "git_status")]),
            reply("", vec![call("c1", "git_status")]),
            reply("", vec![call("c2", "git_status")]),
        ]);
        let executor = FakeExecutor::new();
        let seen = Collect::default();
        let err = run_tool_loop(&mut backend, &executor, &Approve(true), 1, &seen)
            .await
            .expect_err("лимит");
        assert!(matches!(
            err.error.downcast_ref::<AgentError>(),
            Some(AgentError::ToolLoopLimit { iterations: 1 })
        ));
        assert_eq!(err.executed, 1);
    }

    #[tokio::test]
    async fn server_unavailable_aborts_turn_with_partial_messages() {
        let mut backend = Script::new(vec![reply("", vec![call("c0", "git_status")])]);
        let mut executor = FakeExecutor::new();
        executor.fail_with = Some(|| {
            AgentError::ToolServerUnavailable {
                server: "git-mcp".into(),
                reason: "упал".into(),
            }
            .into()
        });
        let seen = Collect::default();
        let err = run_tool_loop(&mut backend, &executor, &Approve(true), 8, &seen)
            .await
            .expect_err("сервер недоступен");
        assert_eq!(err.executed, 1);
        assert_eq!(seen.all().len(), 1, "ответ с вызовом сохранён, результата нет");
    }

    #[tokio::test]
    async fn ordinary_tool_error_becomes_result() {
        let mut backend = Script::new(vec![
            reply("", vec![call("c0", "git_status")]),
            reply("ок", vec![]),
        ]);
        let mut executor = FakeExecutor::new();
        executor.fail_with = Some(|| anyhow::anyhow!("нет такого файла"));
        let seen = Collect::default();
        run_tool_loop(&mut backend, &executor, &Approve(true), 8, &seen)
            .await
            .expect("ход");
        assert!(seen.all()[1].content.starts_with("Ошибка инструмента: "));
    }

    #[test]
    fn trailing_results_are_the_last_group() {
        let turn = vec![
            Message::assistant_with_tool_calls("", vec![call("c0", "git_status")]),
            Message::tool_result("c0", "git_status", "a"),
            Message::assistant_with_tool_calls("", vec![call("c1", "git_log")]),
            Message::tool_result("c1", "git_log", "b"),
        ];
        let results = trailing_results(&turn);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "b");
    }
}
