use super::{Agent, Message, Role};
use crate::config::{ChatSettings, Config, ResponseFormat};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

pub struct HttpAgent {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl HttpAgent {
    pub fn from_config(config: &Config) -> Result<Self> {
        let api_key = config
            .api_key
            .clone()
            .context("API key не задан. Выполните: agentcli config set-key <KEY>")?;

        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: config
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        })
    }

    /// Системный промпт: стратегия рассуждения плюс описание формата и
    /// условие завершения ответа (последние — только в кастомном режиме).
    fn system_prompt(settings: &ChatSettings) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(reasoning) = settings.reasoning_prompt() {
            parts.push(reasoning);
        }
        parts.extend(Self::format_prompt_parts(settings.active_response_format()));
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
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
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
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: Option<ApiErrorDetail>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
}

#[derive(Serialize)]
struct RequestLogEntry<'a> {
    id: &'a str,
    timestamp: u64,
    url: &'a str,
    model: &'a str,
    request: serde_json::Value,
}

#[derive(Serialize)]
struct ResponseLogEntry<'a> {
    id: &'a str,
    timestamp: u64,
    status: u16,
    duration_ms: u128,
    response: serde_json::Value,
}

fn logs_dir() -> Result<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("logs");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("не удалось создать директорию логов {}", dir.display()))?;
    Ok(dir)
}

fn append_log_line(file_name: &str, line: &str) {
    let Ok(dir) = logs_dir() else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file_name))
    {
        let _ = writeln!(file, "{line}");
    }
}

fn log_request(entry: &RequestLogEntry) {
    if let Ok(line) = serde_json::to_string(entry) {
        append_log_line("requests.jsonl", &line);
    }
}

fn log_response(entry: &ResponseLogEntry) {
    if let Ok(line) = serde_json::to_string(entry) {
        append_log_line("responses.jsonl", &line);
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn request_id() -> String {
    format!(
        "{}-{:x}",
        unix_timestamp(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    )
}

fn parse_api_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let detail = serde_json::from_str::<ApiErrorBody>(body)
        .ok()
        .and_then(|e| e.error)
        .map(|e| e.message)
        .unwrap_or_else(|| body.to_string());
    anyhow::anyhow!("API вернул ошибку ({status}): {detail}")
}

impl HttpAgent {
    fn build_messages(&self, history: &[Message], settings: &ChatSettings) -> Vec<ChatMessage> {
        let mut messages = Vec::with_capacity(history.len() + 1);
        if let Some(system_content) = Self::system_prompt(settings) {
            messages.push(ChatMessage {
                role: "system",
                content: system_content,
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

    fn build_request<'a>(
        &'a self,
        messages: Vec<ChatMessage>,
        settings: &ChatSettings,
    ) -> ChatRequest<'a> {
        let active_format = settings.active_response_format();
        let sampling = &settings.sampling;
        ChatRequest {
            model: &self.model,
            messages,
            max_tokens: active_format.and_then(|f| f.max_length),
            stop: active_format.and_then(|f| f.stop.clone()),
            temperature: sampling.temperature,
            top_p: sampling.top_p,
            top_k: sampling.top_k,
            frequency_penalty: sampling.frequency_penalty,
            presence_penalty: sampling.presence_penalty,
        }
    }

    async fn send_request(&self, url: &str, request_body: &ChatRequest<'_>) -> Result<String> {
        let id = request_id();
        let request_json = serde_json::to_value(request_body).unwrap_or(serde_json::Value::Null);
        log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url,
            model: &self.model,
            request: request_json,
        });

        let started_at = Instant::now();
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(request_body)
            .send()
            .await
            .context("не удалось отправить запрос к API")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("не удалось прочитать тело ответа")?;
        let duration_ms = started_at.elapsed().as_millis();

        let response_json = serde_json::from_str::<serde_json::Value>(&body)
            .unwrap_or(serde_json::Value::String(body.clone()));
        log_response(&ResponseLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            status: status.as_u16(),
            duration_ms,
            response: response_json,
        });

        if !status.is_success() {
            bail!(parse_api_error(status, &body));
        }

        Ok(body)
    }
}

fn extract_answer(body: &str) -> Result<String> {
    let parsed: ChatResponse =
        serde_json::from_str(body).context("не удалось разобрать ответ API")?;

    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("ответ API не содержит вариантов")
}

#[async_trait]
impl Agent for HttpAgent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<String> {
        let messages = self.build_messages(history, settings);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let request_body = self.build_request(messages, settings);

        let body = self.send_request(&url, &request_body).await?;
        extract_answer(&body)
    }
}
