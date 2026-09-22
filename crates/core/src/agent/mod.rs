pub mod error;
mod local;
pub mod ollama;
pub mod tools;

pub use error::{transport_error, AgentError, MISSING_API_KEY_MESSAGE, UNAUTHORIZED_MESSAGE};
pub use local::OllamaAgent;
pub use ollama::list_models as list_ollama_models;
pub use tools::{close_dangling_tool_calls, ToolCall, ToolSpec};

use crate::config::ResponseFormat;

use crate::config::ChatSettings;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Цепочка рассуждений модели, если модель её вернула.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Телеметрия сообщения: токены, время, скорость.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<MessageMeta>,
    /// Вызовы инструментов, которые запросила модель в этом ответе.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// У сообщения роли `tool`: на какой вызов оно отвечает.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// У сообщения роли `tool`: имя инструмента. Нужно Ollama, который
    /// сопоставляет результат с вызовом по имени, а не по `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            reasoning: None,
            meta: Some(MessageMeta {
                sent_at: Some(now_secs()),
                ..MessageMeta::default()
            }),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content.into())
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content.into())
    }

    /// Ответ модели, запросивший инструменты. `content` обычно пуст.
    pub fn assistant_with_tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls: calls,
            ..Self::plain(Role::Assistant, content.into())
        }
    }

    /// Результат вызова инструмента для модели.
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            tool_name: Some(name.into()),
            ..Self::plain(Role::Tool, content.into())
        }
    }

    /// Сообщение без рассуждения, телеметрии и инструментов.
    pub fn plain(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            reasoning: None,
            meta: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }
}

/// Измеримые характеристики одного обмена с API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMeta {
    /// Токены запроса (весь контекст, отправленный модели).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    /// Токены ответа, включая токены рассуждения.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// Токены, потраченные именно на рассуждение, если API их отделяет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    /// Время от отправки запроса до получения ответа.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Unix-время отправки запроса.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<i64>,
    /// Unix-время получения ответа.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<i64>,
    /// Модель, которой ответили на самом деле: сервис мог выбрать не ту,
    /// что запрошена, и показывать нужно фактическую.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl MessageMeta {
    /// Скорость генерации в токенах в секунду.
    pub fn tokens_per_second(&self) -> Option<f64> {
        let tokens = self.completion_tokens? as f64;
        let seconds = self.duration_ms? as f64 / 1000.0;
        if seconds <= 0.0 {
            return None;
        }
        Some(tokens / seconds)
    }
}

/// Ответ агента вместе с рассуждением и телеметрией.
#[derive(Debug, Clone)]
pub struct AgentReply {
    pub content: String,
    pub reasoning: Option<String>,
    pub meta: MessageMeta,
    /// Модель, которой ответили на самом деле. `None` — источник ответа её не
    /// сообщил, и показывать следует запрошенную.
    pub model: Option<String>,
    /// Результаты стадий конвейера, если ответ пришёл через сервис.
    pub policy: Option<crate::pipeline::PolicyLog>,
    /// Что стратегия контекста сделала при сборке истории, если ответ пришёл
    /// через сервис с чатом (`chat_id`). `None` — разовый вызов без чата или
    /// ответ от локальной модели, которую стратегии контекста не касаются.
    pub context: Option<crate::config::ContextObservability>,
    /// Вызовы инструментов, которые запросила модель. Пустой вектор значит
    /// «ответ окончательный».
    pub tool_calls: Vec<ToolCall>,
}

pub fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    /// Результат вызова инструмента.
    Tool,
}

/// `Send + Sync` нужны, чтобы агента можно было держать в `Arc<dyn Agent>`
/// и делить между одновременными запросами сетевого сервиса.
#[async_trait]
pub trait Agent: Send + Sync {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply>;

    /// Вызов модели с описаниями инструментов.
    ///
    /// Реализация по умолчанию нужна, чтобы провайдер без поддержки
    /// инструментов (и тестовые подделки, и судья) не писал ни строчки:
    /// пустой список сводится к `ask`, непустой — явный отказ.
    async fn ask_with_tools(
        &self,
        history: &[Message],
        settings: &ChatSettings,
        tools: &[ToolSpec],
    ) -> Result<AgentReply> {
        if tools.is_empty() {
            return self.ask(history, settings).await;
        }
        Err(AgentError::ToolsUnsupported {
            model: None,
            request_id: None,
        }
        .into())
    }
}

/// Системный промпт запроса: стратегия рассуждения плюс описание формата и
/// условие завершения ответа (последние — только в кастомном режиме).
///
/// Функция общая для всех провайдеров: тело запроса у них разное, а правила
/// сборки системного сообщения — одни и те же.
pub fn system_prompt(settings: &ChatSettings) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(reasoning) = settings.reasoning_prompt() {
        parts.push(reasoning);
    }
    parts.extend(format_prompt_parts(settings.active_response_format()));
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn format_prompt_parts(format: Option<&ResponseFormat>) -> Vec<String> {
    let Some(format) = format else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    if let Some(description) = &format.description {
        parts.push(format!("Формат ответа: {description}"));
    }
    if let Some(instruction) = &format.stop_instruction {
        parts.push(format!("Условие завершения ответа: {instruction}"));
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_with_tool_calls_round_trips() {
        let message = Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call_0".into(),
                name: "git_status".into(),
                arguments: json!({}),
            }],
        );
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["tool_calls"][0]["name"], "git_status");
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back.tool_calls, message.tool_calls);

        let result = Message::tool_result("call_0", "git_status", "clean");
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_call_id"], "call_0");
        assert_eq!(value["tool_name"], "git_status");
    }

    #[test]
    fn message_without_tools_serializes_without_new_fields() {
        let value = serde_json::to_value(Message::assistant("ok")).unwrap();
        assert!(value.get("tool_calls").is_none());
        assert!(value.get("tool_call_id").is_none());
        assert!(value.get("tool_name").is_none());
    }

    #[test]
    fn old_message_json_without_tool_fields_parses() {
        let message: Message =
            serde_json::from_value(json!({ "role": "assistant", "content": "привет" })).unwrap();
        assert!(message.tool_calls.is_empty());
        assert!(message.tool_call_id.is_none());
        assert!(message.tool_name.is_none());
    }
}
