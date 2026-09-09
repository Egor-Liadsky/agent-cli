//! Локальные модели через Ollama.
//!
//! Используется нативный API (`/api/chat`, `/api/tags`), а не
//! OpenAI-совместимый слой: нативный отдаёт цепочку рассуждения
//! (`message.thinking`), счётчики токенов и `top_k`, то есть всё, что
//! приложение уже показывает для облачных моделей.

use super::{AgentReply, Message, MessageMeta, Role};
use crate::config::{ChatSettings, ThinkingMode};
use super::error::{transport_error, AgentError};
use crate::logging::{request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    /// Ответ приходит одним JSON: приложение не рисует потоковый вывод.
    stream: bool,
    /// Встроенное размышление модели. В режиме «Авто» поле не отправляется:
    /// модели без поддержки thinking отвечают на него ошибкой.
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
    #[serde(skip_serializing_if = "Options::is_empty")]
    options: Options,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

/// Параметры генерации Ollama. Незаданные поля не отправляются — модель
/// использует свои значения по умолчанию.
#[derive(Serialize, Default)]
struct Options {
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
    /// Ограничение длины ответа в токенах (аналог max_tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
}

impl Options {
    fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.top_p.is_none()
            && self.top_k.is_none()
            && self.frequency_penalty.is_none()
            && self.presence_penalty.is_none()
            && self.num_predict.is_none()
            && self.stop.is_none()
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    #[serde(default)]
    content: String,
    /// Цепочка рассуждений модели, если thinking включён.
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    name: String,
}

/// Клиент для локальных запросов. Прокси из окружения (`HTTP_PROXY`)
/// отключён намеренно: Ollama работает на самой машине, а прокси рвёт
/// долгие ответы больших моделей по своему таймауту (502 с пустым телом).
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_proxy()
}

pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn chat_url(base_url: &str) -> String {
    format!("{}/api/chat", base_url.trim_end_matches('/'))
}

fn tags_url(base_url: &str) -> String {
    format!("{}/api/tags", base_url.trim_end_matches('/'))
}

/// Ошибка Ollama: тело `{"error": "..."}`, иначе — как есть.
fn parse_error(status: reqwest::StatusCode, body: &str) -> AgentError {
    let message = if body.trim().is_empty() {
        "пустое тело ответа. Обычно так отвечает HTTP-прокси или обратный прокси \
         перед Ollama, а не он сам: проверьте адрес"
            .to_string()
    } else {
        serde_json::from_str::<ErrorBody>(body)
            .map(|e| e.error)
            .unwrap_or_else(|_| body.to_string())
    };
    AgentError::provider(status.as_u16(), message)
}

/// Не удалось соединиться — почти всегда это «сервер не запущен».
fn connection_error(base_url: &str, err: reqwest::Error) -> AgentError {
    transport_error(
        &format!("не удалось связаться с Ollama по адресу {base_url}"),
        err,
    )
}

/// Список локально скачанных моделей (`ollama list`).
pub async fn list_models(base_url: &str) -> Result<Vec<String>> {
    let url = tags_url(base_url);
    let response = client()
        .get(&url)
        .send()
        .await
        .map_err(|err| connection_error(base_url, err))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| transport_error("не удалось прочитать список моделей Ollama", err))?;
    if !status.is_success() {
        return Err(parse_error(status, &body).into());
    }
    let parsed: TagsResponse = serde_json::from_str(&body).map_err(|err| {
        AgentError::Decode(format!("не удалось разобрать список моделей Ollama: {err}"))
    })?;
    Ok(parsed.models.into_iter().map(|m| m.name).collect())
}

fn build_messages(system: Option<String>, history: &[Message]) -> Vec<ChatMessage> {
    let mut messages = Vec::with_capacity(history.len() + 1);
    if let Some(content) = system {
        messages.push(ChatMessage {
            role: "system",
            content,
        });
    }
    messages.extend(history.iter().map(|m| ChatMessage {
        role: match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        content: m.content.clone(),
    }));
    messages
}

fn build_options(settings: &ChatSettings) -> Options {
    let format = settings.active_response_format();
    let sampling = &settings.sampling;
    Options {
        temperature: sampling.temperature,
        top_p: sampling.top_p,
        top_k: sampling.top_k,
        frequency_penalty: sampling.frequency_penalty,
        presence_penalty: sampling.presence_penalty,
        num_predict: format.and_then(|f| f.max_length),
        stop: format.and_then(|f| f.stop.clone()),
    }
}

fn think_flag(mode: ThinkingMode) -> Option<bool> {
    match mode {
        ThinkingMode::Auto => None,
        ThinkingMode::Enabled => Some(true),
        ThinkingMode::Disabled => Some(false),
    }
}

/// Один обмен с локальной моделью.
pub async fn chat(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    history: &[Message],
    settings: &ChatSettings,
    system: Option<String>,
    log: &ExchangeLog,
) -> Result<AgentReply> {
    if model.trim().is_empty() {
        anyhow::bail!(
            "модель Ollama не выбрана. Выберите её в настройках чата (Ctrl+P → «Подключение») \
             или выполните: agentcli ollama use <МОДЕЛЬ>"
        );
    }
    let request_body = ChatRequest {
        model,
        messages: build_messages(system, history),
        stream: false,
        think: think_flag(settings.thinking),
        options: build_options(settings),
    };

    let url = chat_url(base_url);
    let id = request_id();
    log.log_request(&RequestLogEntry {
        id: &id,
        timestamp: unix_timestamp(),
        url: &url,
        model,
        request: serde_json::to_value(&request_body).unwrap_or(serde_json::Value::Null),
    });

    let started_at = Instant::now();
    let sent_at = unix_timestamp() as i64;
    let response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await
        .map_err(|err| connection_error(base_url, err))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| transport_error("не удалось прочитать тело ответа Ollama", err))?;
    let duration_ms = started_at.elapsed().as_millis();

    log.log_response(&ResponseLogEntry {
        id: &id,
        timestamp: unix_timestamp(),
        status: status.as_u16(),
        duration_ms,
        response: serde_json::from_str::<serde_json::Value>(&body)
            .unwrap_or(serde_json::Value::String(body.clone())),
    });

    if !status.is_success() {
        return Err(parse_error(status, &body).into());
    }

    let parsed: ChatResponse = serde_json::from_str(&body)
        .map_err(|err| AgentError::Decode(format!("не удалось разобрать ответ Ollama: {err}")))?;
    let total_tokens = match (parsed.prompt_eval_count, parsed.eval_count) {
        (Some(prompt), Some(completion)) => Some(prompt + completion),
        _ => None,
    };
    let meta = MessageMeta {
        prompt_tokens: parsed.prompt_eval_count,
        completion_tokens: parsed.eval_count,
        total_tokens,
        // Ollama не разделяет токены рассуждения и ответа
        reasoning_tokens: None,
        duration_ms: Some(duration_ms as u64),
        sent_at: Some(sent_at),
        received_at: Some(unix_timestamp() as i64),
        model: Some(model.to_string()),
    };
    let reasoning = parsed
        .message
        .thinking
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());

    Ok(AgentReply {
        content: parsed.message.content,
        reasoning,
        meta,
        model: Some(model.to_string()),
        policy: None,
    })
}
