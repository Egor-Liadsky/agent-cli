//! Клиентская сторона сервиса `agentd`.
//!
//! Крейт даёт реализацию трейта `Agent` поверх HTTP-контракта сервиса
//! (`POST /v1/chat`, `GET /v1/models`), чтобы консольный клиент получал ответ
//! облачной модели, не зная ключа провайдера: ключ принадлежит сервису.

use agentcore::agent::{
    transport_error, Agent, AgentError, AgentReply, Message, MessageMeta, Role,
};
use agentcore::config::{ChatSettings, ReasoningMode, ThinkingMode};
use agentcore::pipeline::PolicyLog;
use agentcore::logging::{
    request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Заголовок с идентификатором запроса: сервис дублирует его и в теле ошибки,
/// но при пустом теле остаётся только заголовок.
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";

pub struct ServerAgent {
    client: reqwest::Client,
    server_url: String,
    /// Пустая строка означает «токен не задан»: сервис с пустым списком
    /// клиентских токенов аутентификацию не проверяет.
    token: String,
    /// Модель по умолчанию: используется, если у чата нет своей.
    model: String,
    /// Журнал обмена с сервисом. Назначение задаёт вызывающая сторона.
    log: Arc<ExchangeLog>,
    /// Подсказка вызывающей стороны в сообщении об отказе аутентификации.
    unauthorized_hint: Option<String>,
}

impl ServerAgent {
    /// Адрес сервиса, клиентский токен и модель по умолчанию задаёт
    /// вызывающая сторона: крейт не читает пользовательский конфиг.
    pub fn new(
        server_url: impl Into<String>,
        token: impl Into<String>,
        model: impl Into<String>,
        log: Arc<ExchangeLog>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            server_url: server_url.into(),
            token: token.into(),
            model: model.into(),
            log,
            unauthorized_hint: None,
        }
    }

    /// Подсказка, которую вызывающая сторона добавляет к нейтральному
    /// сообщению об отказе аутентификации.
    pub fn with_unauthorized_hint(mut self, hint: impl Into<String>) -> Self {
        self.unauthorized_hint = Some(hint.into());
        self
    }

    /// Таймаут запроса к сервису. Его истечение даёт [`AgentError::Timeout`],
    /// а не безымянную транспортную ошибку.
    pub fn with_request_timeout(mut self, timeout: Duration) -> anyhow::Result<Self> {
        self.client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(self)
    }

    /// Модель запроса: своя у чата, иначе модель по умолчанию. Пустое
    /// значение не отправляется: модель выбирает сервис.
    fn model_for(&self, settings: &ChatSettings) -> Option<String> {
        settings
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| Some(self.model.clone()).filter(|m| !m.trim().is_empty()))
    }

    fn chat_url(&self) -> String {
        format!("{}/v1/chat", self.server_url.trim_end_matches('/'))
    }

    /// Заголовок аутентификации добавляется только при заданном токене.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.trim().is_empty() {
            request
        } else {
            request.bearer_auth(self.token.trim())
        }
    }

    fn unavailable(&self, err: reqwest::Error) -> AgentError {
        transport_error(
            &format!("сервис недоступен по адресу {}", self.server_url),
            err,
        )
    }
}

/// Тело запроса к `POST /v1/chat`. Поля `api_key` здесь нет намеренно: ключ
/// провайдера принадлежит сервису, и тело с этим полем он отклоняет как `400`.
///
/// С `chat_id` сервис берёт историю из чата и сам записывает обмен, поэтому
/// в теле уходит только новая реплика: `messages` вместе с `chat_id` контракт
/// не принимает.
#[derive(Serialize)]
struct ChatRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<Vec<ChatMessage>>,
    settings: ChatSettingsPayload,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct ChatSettingsPayload {
    /// Провайдер задаётся явно: у сервиса свои умолчания.
    provider: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    reasoning: ReasoningMode,
    thinking: ThinkingMode,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    experts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormatPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_context_tokens: Option<u32>,
}

#[derive(Serialize)]
pub(crate) struct ResponseFormatPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_instruction: Option<String>,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    request_id: Option<String>,
    content: String,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: UsagePayload,
    #[serde(default)]
    timing: TimingPayload,
    /// Результаты стадий конвейера сервиса.
    #[serde(default)]
    policy: Option<PolicyLog>,
}

#[derive(Deserialize, Default)]
struct UsagePayload {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Deserialize, Default)]
struct TimingPayload {
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    sent_at: Option<i64>,
    #[serde(default)]
    received_at: Option<i64>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    request_id: Option<String>,
}

/// Ошибка сервиса в терминах ядра. Разбор идёт по коду состояния: коды
/// задокументированы контрактом `/v1`, а `code` из тела попадает в текст.
pub(crate) fn parse_service_error(
    status: reqwest::StatusCode,
    body: &str,
    header_request_id: Option<String>,
    unauthorized_hint: Option<String>,
) -> AgentError {
    let envelope = serde_json::from_str::<ErrorEnvelope>(body).ok().map(|e| e.error);
    let request_id = envelope
        .as_ref()
        .and_then(|e| e.request_id.clone())
        .filter(|id| !id.trim().is_empty())
        .or(header_request_id);
    let message = envelope
        .as_ref()
        .map(|e| e.message.clone())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| body.trim().to_string());
    let code = envelope.map(|e| e.code).unwrap_or_default();

    match status.as_u16() {
        401 => AgentError::Unauthorized {
            hint: unauthorized_hint,
            request_id,
        },
        400 | 413 => AgentError::InvalidRequest {
            message,
            request_id,
        },
        422 => AgentError::PolicyRejected {
            code: if code.is_empty() {
                "policy_rejected".to_string()
            } else {
                code
            },
            reason: message,
            request_id,
        },
        429 => AgentError::RateLimited {
            message,
            request_id,
        },
        504 => AgentError::Timeout { request_id },
        other => AgentError::Provider {
            status: other,
            message,
            request_id,
        },
    }
}

pub(crate) fn header_request_id(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
        .filter(|value| !value.trim().is_empty())
}

impl ServerAgent {
    fn build_request(&self, history: &[Message], settings: &ChatSettings) -> ChatRequest {
        // Системное сообщение клиент не отправляет: контракт `/v1` знает
        // только роли `user` и `assistant`, а системный промпт сервис
        // собирает сам из полученных настроек чата.
        let mut messages = Vec::with_capacity(history.len());
        messages.extend(history.iter().map(|m| ChatMessage {
            role: match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            },
            content: m.content.clone(),
        }));
        self.build_body(None, None, Some(messages), settings)
    }

    fn build_body(
        &self,
        chat_id: Option<String>,
        prompt: Option<String>,
        messages: Option<Vec<ChatMessage>>,
        settings: &ChatSettings,
    ) -> ChatRequest {
        let sampling = &settings.sampling;
        ChatRequest {
            chat_id,
            prompt,
            messages,
            settings: ChatSettingsPayload {
                provider: "cloud",
                model: self.model_for(settings),
                reasoning: settings.reasoning,
                thinking: settings.thinking,
                experts: settings.experts.clone(),
                response_format: settings.active_response_format().map(|format| {
                    ResponseFormatPayload {
                        description: format.description.clone(),
                        max_length: format.max_length,
                        stop: format.stop.clone(),
                        stop_instruction: format.stop_instruction.clone(),
                    }
                }),
                temperature: sampling.temperature,
                top_p: sampling.top_p,
                top_k: sampling.top_k,
                frequency_penalty: sampling.frequency_penalty,
                presence_penalty: sampling.presence_penalty,
                max_context_tokens: settings.max_context_tokens,
            },
        }
    }
}

impl ServerAgent {
    /// Диалог в чате сервиса: историю сервис берёт из хранилища сам, а обмен
    /// записывает одной транзакцией после ответа модели. Клиент отправляет
    /// только новую реплику (specs/client-chat-storage, «Реплики чата
    /// попадают в сервис»).
    pub async fn ask_in_chat(
        &self,
        chat_id: &str,
        prompt: &str,
        settings: &ChatSettings,
    ) -> Result<AgentReply> {
        let body = self.build_body(
            Some(chat_id.to_string()),
            Some(prompt.to_string()),
            None,
            settings,
        );
        self.exchange(body).await
    }
}

#[async_trait]
impl Agent for ServerAgent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply> {
        let request_body = self.build_request(history, settings);
        self.exchange(request_body).await
    }
}

impl ServerAgent {
    /// Один обмен с сервисом: журнал, разбор ошибок и сборка ответа агента.
    async fn exchange(&self, request_body: ChatRequest) -> Result<AgentReply> {
        let url = self.chat_url();
        let id = request_id();
        self.log.log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url: &url,
            model: request_body.settings.model.as_deref().unwrap_or(""),
            request: serde_json::to_value(&request_body).unwrap_or(serde_json::Value::Null),
        });

        let started_at = Instant::now();
        let sent_at = unix_timestamp() as i64;
        let response = self
            .authorize(self.client.post(&url))
            .json(&request_body)
            .send()
            .await
            .map_err(|err| self.unavailable(err))?;

        let status = response.status();
        let header_id = header_request_id(&response);
        let body = response
            .text()
            .await
            .map_err(|err| transport_error("не удалось прочитать ответ сервиса", err))?;
        let duration_ms = started_at.elapsed().as_millis();

        self.log.log_response(&ResponseLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            status: status.as_u16(),
            duration_ms,
            response: serde_json::from_str::<serde_json::Value>(&body)
                .unwrap_or(serde_json::Value::String(body.clone())),
        });

        if !status.is_success() {
            return Err(parse_service_error(
                status,
                &body,
                header_id,
                self.unauthorized_hint.clone(),
            )
            .into());
        }

        let parsed: ChatResponse = serde_json::from_str(&body).map_err(|err| {
            AgentError::Decode(format!("не удалось разобрать ответ сервиса: {err}"))
        })?;

        // Телеметрия берётся из ответа сервиса: он измерял обмен с провайдером.
        // Длительность своего запроса используется, только если её не прислали.
        let meta = MessageMeta {
            prompt_tokens: parsed.usage.prompt_tokens,
            completion_tokens: parsed.usage.completion_tokens,
            total_tokens: parsed.usage.total_tokens,
            reasoning_tokens: parsed.usage.reasoning_tokens,
            duration_ms: parsed.timing.duration_ms.or(Some(duration_ms as u64)),
            sent_at: parsed.timing.sent_at.or(Some(sent_at)),
            received_at: parsed.timing.received_at.or(Some(unix_timestamp() as i64)),
            model: parsed.model.clone().filter(|m| !m.trim().is_empty()),
        };
        // Идентификатор запроса нужен только при ошибке: успешный ответ
        // клиент по журналу сервиса не разыскивает.
        let _ = parsed.request_id;

        Ok(AgentReply {
            content: parsed.content,
            reasoning: parsed
                .reasoning
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty()),
            meta,
            model: parsed.model.filter(|m| !m.trim().is_empty()),
            policy: parsed.policy,
        })
    }
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    models: Vec<String>,
}

/// Список моделей, разрешённых сервисом (`GET /v1/models`).
///
/// Список принадлежит сервису: встроенного набора у клиента больше нет,
/// поэтому недоступность сервиса — ошибка, а не пустой список.
pub async fn list_models(server_url: &str, token: &str) -> Result<Vec<String>> {
    let url = format!("{}/v1/models", server_url.trim_end_matches('/'));
    let request = reqwest::Client::new().get(&url);
    let request = if token.trim().is_empty() {
        request
    } else {
        request.bearer_auth(token.trim())
    };
    let response = request.send().await.map_err(|err| {
        transport_error(&format!("сервис недоступен по адресу {server_url}"), err)
    })?;
    let status = response.status();
    let header_id = header_request_id(&response);
    let body = response
        .text()
        .await
        .map_err(|err| transport_error("не удалось прочитать список моделей", err))?;
    if !status.is_success() {
        return Err(parse_service_error(status, &body, header_id, None).into());
    }
    let parsed: ModelsResponse = serde_json::from_str(&body).map_err(|err| {
        AgentError::Decode(format!("не удалось разобрать список моделей сервиса: {err}"))
    })?;
    Ok(parsed.models)
}

mod chats;
pub use chats::{ChatHistory, ChatSummary, ChatsClient, StoredMessage};

#[cfg(test)]
mod tests;
