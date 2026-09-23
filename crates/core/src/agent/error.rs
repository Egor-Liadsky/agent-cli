//! Типизированная ошибка вызова модели.
//!
//! Трейт [`Agent`](super::Agent) продолжает возвращать `anyhow::Error`, но
//! ядро всегда строит её из `AgentError`. Так вызывающая сторона (например
//! сетевой сервис) отличает таймаут от ошибки провайдера через
//! `err.downcast_ref::<AgentError>()`, не разбирая текст сообщения.

use std::fmt;

/// Нейтральный текст об отсутствующем ключе: без команд консольного клиента.
pub const MISSING_API_KEY_MESSAGE: &str = "API key не задан";

/// Нейтральный текст об отказе аутентификации: без команд консольного клиента.
pub const UNAUTHORIZED_MESSAGE: &str = "сервис не принял клиентский токен";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// Провайдер не ответил в отведённое время.
    Timeout { request_id: Option<String> },
    /// Провайдер ответил кодом ошибки.
    Provider {
        status: u16,
        message: String,
        request_id: Option<String>,
    },
    /// Запрос не дошёл до провайдера: сеть, DNS, TLS.
    Transport(String),
    /// Ключ провайдера не задан. `hint` задаёт вызывающая сторона.
    MissingApiKey { hint: Option<String> },
    /// Ответ провайдера не разобран.
    Decode(String),
    /// Клиентский токен отсутствует или не известен сервису.
    Unauthorized {
        hint: Option<String>,
        request_id: Option<String>,
    },
    /// Запрос отвергнут как неправильно составленный.
    InvalidRequest {
        message: String,
        request_id: Option<String>,
    },
    /// Запрос отклонён стадией конвейера: код и причина отказа.
    PolicyRejected {
        code: String,
        reason: String,
        request_id: Option<String>,
    },
    /// Превышен предел нагрузки.
    RateLimited {
        message: String,
        request_id: Option<String>,
    },
    /// Модель или провайдер не умеют вызывать инструменты.
    ToolsUnsupported {
        model: Option<String>,
        request_id: Option<String>,
    },
    /// Сервер инструментов (MCP) не запустился, не прошёл рукопожатие или
    /// упал и не перезапустился.
    ToolServerUnavailable { server: String, reason: String },
    /// Модель продолжает вызывать инструменты после исчерпания лимита
    /// итераций и финального запроса без инструментов.
    ToolLoopLimit { iterations: u32 },
}

impl AgentError {
    pub fn missing_api_key(hint: Option<String>) -> Self {
        AgentError::MissingApiKey { hint }
    }

    pub fn timeout() -> Self {
        AgentError::Timeout { request_id: None }
    }

    pub fn provider(status: u16, message: impl Into<String>) -> Self {
        AgentError::Provider {
            status,
            message: message.into(),
            request_id: None,
        }
    }

    /// Идентификатор запроса виден в тексте ошибки: без него разбирательство
    /// по журналу сервиса невозможно.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            AgentError::Timeout { request_id }
            | AgentError::Provider { request_id, .. }
            | AgentError::Unauthorized { request_id, .. }
            | AgentError::InvalidRequest { request_id, .. }
            | AgentError::PolicyRejected { request_id, .. }
            | AgentError::RateLimited { request_id, .. }
            | AgentError::ToolsUnsupported { request_id, .. } => request_id.as_deref(),
            AgentError::Transport(_)
            | AgentError::MissingApiKey { .. }
            | AgentError::Decode(_)
            | AgentError::ToolServerUnavailable { .. }
            | AgentError::ToolLoopLimit { .. } => None,
        }
    }
}

/// Хвост сообщения с идентификатором запроса, если он известен.
fn request_id_suffix(request_id: &Option<String>) -> String {
    match request_id {
        Some(id) if !id.trim().is_empty() => format!(" (request_id: {id})"),
        _ => String::new(),
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Timeout { request_id } => {
                write!(
                    f,
                    "провайдер не ответил в отведённое время{}",
                    request_id_suffix(request_id)
                )
            }
            AgentError::Provider {
                status,
                message,
                request_id,
            } => {
                write!(
                    f,
                    "API вернул ошибку ({status}): {message}{}",
                    request_id_suffix(request_id)
                )
            }
            AgentError::Transport(message) => write!(f, "{message}"),
            AgentError::MissingApiKey { hint } => match hint {
                Some(hint) => write!(f, "{MISSING_API_KEY_MESSAGE}. {hint}"),
                None => write!(f, "{MISSING_API_KEY_MESSAGE}"),
            },
            AgentError::Decode(message) => write!(f, "{message}"),
            AgentError::Unauthorized { hint, request_id } => {
                let suffix = request_id_suffix(request_id);
                match hint {
                    Some(hint) => write!(f, "{UNAUTHORIZED_MESSAGE}. {hint}{suffix}"),
                    None => write!(f, "{UNAUTHORIZED_MESSAGE}{suffix}"),
                }
            }
            AgentError::InvalidRequest {
                message,
                request_id,
            } => write!(
                f,
                "сервис отклонил запрос: {message}{}",
                request_id_suffix(request_id)
            ),
            AgentError::PolicyRejected {
                code,
                reason,
                request_id,
            } => write!(
                f,
                "запрос отклонён политикой сервиса ({code}): {reason}{}",
                request_id_suffix(request_id)
            ),
            AgentError::RateLimited {
                message,
                request_id,
            } => write!(
                f,
                "превышен предел нагрузки: {message}{}",
                request_id_suffix(request_id)
            ),
            AgentError::ToolsUnsupported { model, request_id } => {
                let suffix = request_id_suffix(request_id);
                match model {
                    Some(model) => write!(
                        f,
                        "модель {model} не поддерживает вызов инструментов{suffix}"
                    ),
                    None => write!(f, "модель не поддерживает вызов инструментов{suffix}"),
                }
            }
            AgentError::ToolServerUnavailable { server, reason } => {
                write!(f, "сервер инструментов {server} недоступен: {reason}")
            }
            AgentError::ToolLoopLimit { iterations } => write!(
                f,
                "модель продолжает вызывать инструменты после {iterations} итераций"
            ),
        }
    }
}

impl std::error::Error for AgentError {}

/// Ошибка `reqwest` в терминах ядра: таймаут отделён от прочего транспорта.
pub fn transport_error(context: &str, err: reqwest::Error) -> AgentError {
    if err.is_timeout() {
        AgentError::timeout()
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

    #[test]
    fn tool_errors_downcast_from_anyhow() {
        let errors = [
            AgentError::ToolsUnsupported {
                model: Some("gemma".into()),
                request_id: Some("req-1".into()),
            },
            AgentError::ToolServerUnavailable {
                server: "agentcli-git-mcp".into(),
                reason: "не найден agentcli-git-mcp".into(),
            },
            AgentError::ToolLoopLimit { iterations: 8 },
        ];
        for error in errors {
            let wrapped: anyhow::Error = error.clone().into();
            assert_eq!(wrapped.downcast_ref::<AgentError>(), Some(&error));
        }
    }

    #[test]
    fn tools_unsupported_exposes_request_id() {
        let error = AgentError::ToolsUnsupported {
            model: None,
            request_id: Some("req-7".into()),
        };
        assert_eq!(error.request_id(), Some("req-7"));
        assert!(error.to_string().contains("req-7"));
        assert_eq!(AgentError::ToolLoopLimit { iterations: 3 }.request_id(), None);
    }
}
