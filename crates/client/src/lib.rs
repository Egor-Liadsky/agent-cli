//! Клиентская сторона сервиса `agentd`.
//!
//! Крейт даёт реализацию трейта `Agent` поверх HTTP-контракта сервиса
//! (`POST /v1/chat`, `GET /v1/models`), чтобы консольный клиент получал ответ
//! облачной модели, не зная ключа провайдера: ключ принадлежит сервису.

use agentcore::agent::{
    transport_error, Agent, AgentError, AgentReply, Message, MessageMeta, Role, ToolCall, ToolSpec,
};
use agentcore::config::{ChatSettings, ContextStrategy, ReasoningMode, ThinkingMode};
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
    /// Инструменты этого вызова. Пустой список не отправляется: старый
    /// сервис поле не знает, а без инструментов оно не нужно.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpec>,
    /// Результаты вызовов — продолжение хода в чате вместо `prompt`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_results: Vec<ToolResultPayload>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Serialize)]
struct ToolResultPayload {
    tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
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
    /// Разовое переопределение поверх сохранённого лимита чата: незаданное
    /// поле опускается намеренно, чтобы не снимать лимит, сохранённый на
    /// сервисе. Асимметрия с `ChatSettingsUpdate` в `chats.rs`, где то же
    /// поле отправляется всегда, включая `null`, — часть контракта, а не
    /// случайность (specs/chat-context-limit, «Отправка лимита клиентом»).
    #[serde(skip_serializing_if = "Option::is_none")]
    max_context_tokens: Option<u32>,
    /// Разовое переопределение поверх сохранённых настроек компактизации
    /// чата, той же семантикой присутствия поля, что и у
    /// `max_context_tokens` (specs/context-summary, «Настройки компактизации
    /// на уровне чата»).
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_keep_messages: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_step_messages: Option<u32>,
    /// Разовое переопределение стратегии контекста для этого запроса:
    /// незаданное поле опускается, и действует стратегия, сохранённая в
    /// чате (specs/context-strategies, «Разовое переопределение стратегии в
    /// запросе»).
    #[serde(skip_serializing_if = "Option::is_none")]
    context_strategy: Option<ContextStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window_messages: Option<u32>,
    /// Разовое переопределение профиля этого запроса: незаданное поле
    /// опускается, и действует профиль, сохранённый в чате
    /// (specs/user-profiles, «Профиль выбирается настройкой чата поверх
    /// операторского умолчания»).
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_id: Option<String>,
    /// Разовое переопределение состояния задачи для этого запроса
    /// (specs/task-state, «Операторские умолчания и клиентские
    /// переключатели»): незаданное поле опускается, и действует настройка,
    /// сохранённая в чате.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_state_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_state_auto_enabled: Option<bool>,
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
    /// Что сделала стратегия контекста при сборке истории. `null` — запрос
    /// без чата, компактизации/стратегии не подлежит.
    #[serde(default)]
    context: Option<ContextPayload>,
    /// Вызовы инструментов. Старый сервис поля не присылает — это то же
    /// самое, что пустой список: ответ окончательный.
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

/// Блок наблюдаемости `context` ответа `POST /v1/chat`. Поля, не имеющие
/// смысла для действующей стратегии, сервис не отправляет — здесь это
/// выражено через `Option`, а не через ноль/`false`.
#[derive(Deserialize, Default)]
struct ContextPayload {
    #[serde(default)]
    strategy: Option<ContextStrategy>,
    #[serde(default)]
    sent_messages: Option<u32>,
    #[serde(default)]
    dropped_messages: Option<u32>,
    #[serde(default)]
    replaced_messages: Option<u32>,
    #[serde(default)]
    summary_built: Option<bool>,
    #[serde(default)]
    facts_applied: Option<u32>,
    #[serde(default)]
    facts_updated: Option<bool>,
    #[serde(default)]
    branch_id: Option<String>,
    #[serde(default)]
    memory_long_term_entries: Option<u32>,
    #[serde(default)]
    memory_long_term_chars: Option<u32>,
    #[serde(default)]
    memory_working_entries: Option<u32>,
    #[serde(default)]
    memory_working_chars: Option<u32>,
    #[serde(default)]
    memory_short_term_messages: Option<u32>,
    #[serde(default)]
    memory_short_term_chars: Option<u32>,
    #[serde(default)]
    memory_router_applied_set: Option<u32>,
    #[serde(default)]
    memory_router_applied_update: Option<u32>,
    #[serde(default)]
    memory_router_applied_delete: Option<u32>,
    #[serde(default)]
    memory_router_rejected: Option<u32>,
    #[serde(default)]
    task_stage: Option<String>,
    #[serde(default)]
    task_step: Option<String>,
    #[serde(default)]
    task_expected_action: Option<String>,
    #[serde(default)]
    task_paused: Option<bool>,
    #[serde(default)]
    task_tracker_applied: Option<u32>,
    #[serde(default)]
    task_tracker_rejected: Option<u32>,
}

impl From<ContextPayload> for agentcore::config::ContextObservability {
    fn from(payload: ContextPayload) -> Self {
        Self {
            strategy: payload.strategy,
            sent_messages: payload.sent_messages,
            dropped_messages: payload.dropped_messages,
            replaced_messages: payload.replaced_messages,
            summary_built: payload.summary_built,
            facts_applied: payload.facts_applied,
            facts_updated: payload.facts_updated,
            branch_id: payload.branch_id,
            memory_long_term_entries: payload.memory_long_term_entries,
            memory_long_term_chars: payload.memory_long_term_chars,
            memory_working_entries: payload.memory_working_entries,
            memory_working_chars: payload.memory_working_chars,
            memory_short_term_messages: payload.memory_short_term_messages,
            memory_short_term_chars: payload.memory_short_term_chars,
            memory_router_applied_set: payload.memory_router_applied_set,
            memory_router_applied_update: payload.memory_router_applied_update,
            memory_router_applied_delete: payload.memory_router_applied_delete,
            memory_router_rejected: payload.memory_router_rejected,
            task_stage: payload.task_stage,
            task_step: payload.task_step,
            task_expected_action: payload.task_expected_action,
            task_paused: payload.task_paused,
            task_tracker_applied: payload.task_tracker_applied,
            task_tracker_rejected: payload.task_tracker_rejected,
        }
    }
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
        // Причина различается машинным кодом конверта, а не текстом.
        400 if code == "tools_unsupported" => AgentError::ToolsUnsupported {
            model: None,
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
    fn build_request(&self, history: &[Message], settings: &ChatSettings, tools: &[ToolSpec]) -> ChatRequest {
        // Системное сообщение клиент не отправляет: контракт `/v1` знает
        // только роли `user`, `assistant` и `tool`, а системный промпт
        // сервис собирает сам из полученных настроек чата.
        let mut messages = Vec::with_capacity(history.len());
        messages.extend(history.iter().map(|m| ChatMessage {
            role: match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::Tool => "tool",
            },
            content: m.content.clone(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
            tool_name: m.tool_name.clone(),
        }));
        let mut body = self.build_body(None, None, Some(messages), settings);
        body.tools = tools.to_vec();
        body
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
            tools: Vec::new(),
            tool_results: Vec::new(),
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
                summary_enabled: settings.summary_enabled,
                summary_keep_messages: settings.summary_keep_messages,
                summary_step_messages: settings.summary_step_messages,
                context_strategy: settings.context_strategy,
                context_window_messages: settings.context_window_messages,
                profile_id: settings.profile_id.clone(),
                task_state_enabled: settings.task_state_enabled,
                task_state_auto_enabled: settings.task_state_auto_enabled,
            },
        }
    }
}

impl ServerAgent {
    /// Диалог в чате сервиса: историю сервис берёт из хранилища сам, а обмен
    /// записывает одной транзакцией после ответа модели. Клиент отправляет
    /// только новую реплику (specs/client-chat-storage, «Реплики чата
    /// попадают в сервис»).
    ///
    /// `tools` — инструменты этого хода; ответ с непустым `tool_calls`
    /// продолжается [`ServerAgent::continue_in_chat`].
    pub async fn ask_in_chat(
        &self,
        chat_id: &str,
        prompt: &str,
        settings: &ChatSettings,
        tools: &[ToolSpec],
    ) -> Result<AgentReply> {
        let mut body = self.build_body(
            Some(chat_id.to_string()),
            Some(prompt.to_string()),
            None,
            settings,
        );
        body.tools = tools.to_vec();
        self.exchange(body).await
    }

    /// Продолжение хода с инструментами: результаты вызовов (сообщения роли
    /// `tool`) вместо новой реплики. Сервис сверяет их с вызовами последнего
    /// ответа модели и сам дописывает в чат.
    pub async fn continue_in_chat(
        &self,
        chat_id: &str,
        tool_results: &[Message],
        settings: &ChatSettings,
        tools: &[ToolSpec],
    ) -> Result<AgentReply> {
        let mut body = self.build_body(Some(chat_id.to_string()), None, None, settings);
        body.tools = tools.to_vec();
        body.tool_results = tool_results
            .iter()
            .map(|result| ToolResultPayload {
                tool_call_id: result.tool_call_id.clone().unwrap_or_default(),
                name: result.tool_name.clone(),
                content: result.content.clone(),
            })
            .collect();
        self.exchange(body).await
    }
}

#[async_trait]
impl Agent for ServerAgent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply> {
        self.ask_with_tools(history, settings, &[]).await
    }

    async fn ask_with_tools(
        &self,
        history: &[Message],
        settings: &ChatSettings,
        tools: &[ToolSpec],
    ) -> Result<AgentReply> {
        let request_body = self.build_request(history, settings, tools);
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
            context: parsed.context.map(Into::into),
            tool_calls: parsed.tool_calls,
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

#[derive(Deserialize)]
struct ProfilesResponse {
    #[serde(default)]
    profiles: Vec<ProfileSummaryPayload>,
}

#[derive(Deserialize)]
struct ProfileSummaryPayload {
    id: String,
    name: String,
    built_in: bool,
}

/// Профили, доступные владельцу (`GET /v1/profiles`): встроенные плюс
/// собственные (specs/user-profiles). Только сводка — id/название/признак
/// встроенности, достаточная для поля выбора в настройках чата; полные поля
/// профиля читаются через `chats::Profile` по конкретному id.
pub async fn list_profiles(server_url: &str, token: &str) -> Result<Vec<ProfileChoice>> {
    let url = format!("{}/v1/profiles", server_url.trim_end_matches('/'));
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
        .map_err(|err| transport_error("не удалось прочитать список профилей", err))?;
    if !status.is_success() {
        return Err(parse_service_error(status, &body, header_id, None).into());
    }
    let parsed: ProfilesResponse = serde_json::from_str(&body).map_err(|err| {
        AgentError::Decode(format!("не удалось разобрать список профилей сервиса: {err}"))
    })?;
    Ok(parsed
        .profiles
        .into_iter()
        .map(|p| ProfileChoice { id: p.id, name: p.name, built_in: p.built_in })
        .collect())
}

/// Сводка профиля для поля выбора в настройках чата.
#[derive(Debug, Clone)]
pub struct ProfileChoice {
    pub id: String,
    pub name: String,
    pub built_in: bool,
}

mod chats;
pub use chats::{
    allowed_next_stages, Branch, ChatHistory, ChatSummary, ChatsClient, Fact, LongTermMemoryEntry, Profile,
    StoredMessage, TaskState, TaskTransition, WorkingMemoryEntry,
};

#[cfg(test)]
mod tests;
