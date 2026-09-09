//! Типизированная ошибка вызова модели.
//!
//! Трейт [`Agent`](super::Agent) продолжает возвращать `anyhow::Error`, но
//! ядро всегда строит её из `AgentError`. Так вызывающая сторона (например
//! сетевой сервис) отличает таймаут от ошибки провайдера через
//! `err.downcast_ref::<AgentError>()`, не разбирая текст сообщения.

use std::fmt;

/// Нейтральный текст об отсутствующем ключе: без команд консольного клиента.
pub const MISSING_API_KEY_MESSAGE: &str = "API key не задан";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// Провайдер не ответил в отведённое время.
    Timeout,
    /// Провайдер ответил кодом ошибки.
    Provider { status: u16, message: String },
    /// Запрос не дошёл до провайдера: сеть, DNS, TLS.
    Transport(String),
    /// Ключ провайдера не задан. `hint` задаёт вызывающая сторона.
    MissingApiKey { hint: Option<String> },
    /// Ответ провайдера не разобран.
    Decode(String),
}

impl AgentError {
    pub fn missing_api_key(hint: Option<String>) -> Self {
        AgentError::MissingApiKey { hint }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Timeout => {
                write!(f, "провайдер не ответил в отведённое время")
            }
            AgentError::Provider { status, message } => {
                write!(f, "API вернул ошибку ({status}): {message}")
            }
            AgentError::Transport(message) => write!(f, "{message}"),
            AgentError::MissingApiKey { hint } => match hint {
                Some(hint) => write!(f, "{MISSING_API_KEY_MESSAGE}. {hint}"),
                None => write!(f, "{MISSING_API_KEY_MESSAGE}"),
            },
            AgentError::Decode(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for AgentError {}

/// Ошибка `reqwest` в терминах ядра: таймаут отделён от прочего транспорта.
pub(super) fn transport_error(context: &str, err: reqwest::Error) -> AgentError {
    if err.is_timeout() {
        AgentError::Timeout
    } else {
        AgentError::Transport(format!("{context}: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_message_is_neutral_without_hint() {
        let text = AgentError::missing_api_key(None).to_string();
        assert_eq!(text, MISSING_API_KEY_MESSAGE);
        assert!(!text.contains("agentcli"));
        assert!(!text.contains("Ctrl+P"));
    }

    #[test]
    fn missing_key_message_uses_caller_hint() {
        let text = AgentError::missing_api_key(Some("выполните: agentcli config set-key".into()))
            .to_string();
        assert!(text.starts_with(MISSING_API_KEY_MESSAGE));
        assert!(text.contains("agentcli config set-key"));
    }
}
