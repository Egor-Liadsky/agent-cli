use super::{Agent, Message, Role};
use crate::config::Config;
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
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
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
struct LogEntry<'a> {
    timestamp: u64,
    url: &'a str,
    model: &'a str,
    status: u16,
    duration_ms: u128,
    request: serde_json::Value,
    response: serde_json::Value,
}

fn log_file_path() -> Result<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("logs");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("не удалось создать директорию логов {}", dir.display()))?;
    Ok(dir.join("requests.jsonl"))
}

fn log_request(entry: &LogEntry) {
    let Ok(path) = log_file_path() else { return };
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[async_trait]
impl Agent for HttpAgent {
    async fn ask(&self, history: &[Message]) -> Result<String> {
        let messages = history
            .iter()
            .map(|m| ChatMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: m.content.clone(),
            })
            .collect();

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let request_body = ChatRequest {
            model: &self.model,
            messages,
        };
        let request_json =
            serde_json::to_value(&request_body).unwrap_or(serde_json::Value::Null);

        let started_at = Instant::now();
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&request_body)
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
        log_request(&LogEntry {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            url: &url,
            model: &self.model,
            status: status.as_u16(),
            duration_ms,
            request: request_json,
            response: response_json,
        });

        if !status.is_success() {
            let detail = serde_json::from_str::<ApiErrorBody>(&body)
                .ok()
                .and_then(|e| e.error)
                .map(|e| e.message)
                .unwrap_or(body);
            bail!("API вернул ошибку ({status}): {detail}");
        }

        let parsed: ChatResponse =
            serde_json::from_str(&body).context("не удалось разобрать ответ API")?;

        let answer = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .context("ответ API не содержит вариантов")?;

        Ok(answer)
    }
}
