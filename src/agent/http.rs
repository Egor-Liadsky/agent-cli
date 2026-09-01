use super::{Agent, Message, Role};
use crate::config::Config;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&ChatRequest {
                model: &self.model,
                messages,
            })
            .send()
            .await
            .context("не удалось отправить запрос к API")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("не удалось прочитать тело ответа")?;

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
