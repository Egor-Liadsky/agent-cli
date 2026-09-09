use super::{Agent, AgentReply, Message, MessageMeta, Role};
use crate::config::{ChatSettings, Config, Provider, ResponseFormat, DEFAULT_MODEL};
use super::error::{transport_error, AgentError};
use crate::logging::{request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct HttpAgent {
    client: reqwest::Client,
    /// Пустая строка означает «ключ не задан»: агент создаётся и без ключа,
    /// чтобы его можно было ввести уже в настройках чата.
    api_key: String,
    base_url: String,
    /// Модель по умолчанию: используется, если у чата нет своей.
    model: String,
    /// Отдельный клиент для Ollama: без прокси из окружения.
    ollama_client: reqwest::Client,
    /// Адрес локального Ollama для чатов с провайдером `Ollama`.
    ollama_url: String,
    /// Локальная модель по умолчанию для чатов с провайдером `Ollama`.
    ollama_model: String,
    /// Журнал обмена с провайдером. Назначение задаёт вызывающая сторона.
    log: Arc<ExchangeLog>,
    /// Подсказка вызывающей стороны в сообщении об отсутствующем ключе.
    missing_key_hint: Option<String>,
}

impl HttpAgent {
    pub fn from_config(config: &Config, log: Arc<ExchangeLog>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            api_key: config.api_key.clone().unwrap_or_default(),
            base_url: config.effective_base_url(),
            model: config.effective_model(),
            ollama_client: super::ollama::client(),
            ollama_url: config.effective_ollama_url(),
            ollama_model: config.ollama_model.clone().unwrap_or_default(),
            log,
            missing_key_hint: None,
        })
    }

    /// Подсказка, которую вызывающая сторона добавляет к нейтральному
    /// сообщению об отсутствующем ключе провайдера.
    pub fn with_missing_key_hint(mut self, hint: impl Into<String>) -> Self {
        self.missing_key_hint = Some(hint.into());
        self
    }

    /// Таймаут запроса к провайдеру. Его истечение даёт
    /// [`AgentError::Timeout`], а не безымянную транспортную ошибку.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.client = reqwest::Client::builder().timeout(timeout).build()?;
        self.ollama_client = super::ollama::client_builder().timeout(timeout).build()?;
        Ok(self)
    }

    /// Модель запроса: своя у чата, иначе модель по умолчанию для его
    /// провайдера. У Ollama своя модель по умолчанию и нет встроенной:
    /// список локальных моделей зависит от того, что скачано.
    fn model_for(&self, settings: &ChatSettings) -> String {
        let chat_model = settings.model.clone().filter(|m| !m.trim().is_empty());
        match settings.provider {
            Provider::Cloud => chat_model.unwrap_or_else(|| {
                if self.model.trim().is_empty() {
                    DEFAULT_MODEL.to_string()
                } else {
                    self.model.clone()
                }
            }),
            Provider::Ollama => chat_model.unwrap_or_else(|| self.ollama_model.clone()),
        }
    }

    /// Системный промпт: стратегия рассуждения плюс описание формата и
    /// условие завершения ответа (последние — только в кастомном режиме).
    pub(super) fn system_prompt(settings: &ChatSettings) -> Option<String> {
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
    /// Явное включение/выключение встроенного размышления модели.
    /// Не отправляется в режиме «Авто»: не все провайдеры знают это поле.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
}

#[derive(Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
    /// Цепочка рассуждений: DeepSeek отдаёт её в `reasoning_content`,
    /// OpenAI-совместимые прокси — в `reasoning`.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: Option<ApiErrorDetail>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
}

fn parse_api_error(status: reqwest::StatusCode, body: &str) -> AgentError {
    let message = serde_json::from_str::<ApiErrorBody>(body)
        .ok()
        .and_then(|e| e.error)
        .map(|e| e.message)
        .unwrap_or_else(|| body.to_string());
    AgentError::Provider {
        status: status.as_u16(),
        message,
    }
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
        &self,
        messages: Vec<ChatMessage>,
        settings: &ChatSettings,
        model: &'a str,
    ) -> ChatRequest<'a> {
        let active_format = settings.active_response_format();
        let sampling = &settings.sampling;
        ChatRequest {
            model,
            messages,
            max_tokens: active_format.and_then(|f| f.max_length),
            stop: active_format.and_then(|f| f.stop.clone()),
            temperature: sampling.temperature,
            top_p: sampling.top_p,
            top_k: sampling.top_k,
            frequency_penalty: sampling.frequency_penalty,
            presence_penalty: sampling.presence_penalty,
            thinking: settings.thinking.api_value().map(|kind| Thinking { kind }),
        }
    }

    async fn send_request(
        &self,
        url: &str,
        request_body: &ChatRequest<'_>,
    ) -> Result<(String, MessageMeta)> {
        if self.api_key.trim().is_empty() {
            return Err(AgentError::missing_api_key(self.missing_key_hint.clone()).into());
        }
        let id = request_id();
        let request_json = serde_json::to_value(request_body).unwrap_or(serde_json::Value::Null);
        self.log.log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url,
            model: request_body.model,
            request: request_json,
        });

        let started_at = Instant::now();
        let sent_at = unix_timestamp() as i64;
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(request_body)
            .send()
            .await
            .map_err(|err| transport_error("не удалось отправить запрос к API", err))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| transport_error("не удалось прочитать тело ответа", err))?;
        let duration_ms = started_at.elapsed().as_millis();

        let response_json = serde_json::from_str::<serde_json::Value>(&body)
            .unwrap_or(serde_json::Value::String(body.clone()));
        self.log.log_response(&ResponseLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            status: status.as_u16(),
            duration_ms,
            response: response_json,
        });

        if !status.is_success() {
            return Err(parse_api_error(status, &body).into());
        }

        let meta = MessageMeta {
            duration_ms: Some(duration_ms as u64),
            sent_at: Some(sent_at),
            received_at: Some(unix_timestamp() as i64),
            ..MessageMeta::default()
        };
        Ok((body, meta))
    }
}

fn extract_answer(body: &str, mut meta: MessageMeta) -> Result<AgentReply> {
    let parsed: ChatResponse = serde_json::from_str(body)
        .map_err(|err| AgentError::Decode(format!("не удалось разобрать ответ API: {err}")))?;

    if let Some(usage) = parsed.usage {
        meta.prompt_tokens = usage.prompt_tokens;
        meta.completion_tokens = usage.completion_tokens;
        meta.total_tokens = usage.total_tokens;
        meta.reasoning_tokens = usage
            .completion_tokens_details
            .and_then(|d| d.reasoning_tokens);
    }

    let message = parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message)
        .ok_or_else(|| AgentError::Decode("ответ API не содержит вариантов".to_string()))?;

    let reasoning = message
        .reasoning_content
        .or(message.reasoning)
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());

    Ok(AgentReply {
        content: message.content,
        reasoning,
        meta,
    })
}

#[async_trait]
impl Agent for HttpAgent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply> {
        if settings.provider == Provider::Ollama {
            return super::ollama::chat(
                &self.ollama_client,
                &self.ollama_url,
                &self.model_for(settings),
                history,
                settings,
                Self::system_prompt(settings),
                &self.log,
            )
            .await;
        }
        let messages = self.build_messages(history, settings);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let model = self.model_for(settings);
        let request_body = self.build_request(messages, settings, &model);

        let (body, meta) = self.send_request(&url, &request_body).await?;
        extract_answer(&body, meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Одноразовый сервер: принимает соединение и отвечает статусом и телом.
    /// `None` — не отвечает вовсе, чтобы сработал таймаут клиента.
    async fn stub_provider(response: Option<(u16, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("порт");
        let addr = listener.local_addr().expect("адрес");
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer).await;
            match response {
                Some((status, body)) => {
                    let head = format!(
                        "HTTP/1.1 {status} STATUS\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.flush().await;
                }
                // Держим соединение открытым: клиент должен упереться
                // в собственный таймаут.
                None => tokio::time::sleep(Duration::from_secs(30)).await,
            }
        });
        format!("http://{addr}")
    }

    fn agent(base_url: String) -> HttpAgent {
        let config = Config {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            ..Config::default()
        };
        HttpAgent::from_config(&config, Arc::new(ExchangeLog::disabled()))
            .expect("агент")
            .with_request_timeout(Duration::from_millis(300))
            .expect("таймаут")
    }

    #[tokio::test]
    async fn provider_error_is_typed() {
        let base_url =
            stub_provider(Some((500, r#"{"error":{"message":"внутренняя ошибка"}}"#))).await;

        let history = [Message::user("привет")];
        let err = agent(base_url)
            .ask(&history, &ChatSettings::default())
            .await
            .expect_err("ожидалась ошибка провайдера");

        match err.downcast_ref::<AgentError>() {
            Some(AgentError::Provider { status, message }) => {
                assert_eq!(*status, 500);
                assert_eq!(message, "внутренняя ошибка");
            }
            other => panic!("ожидался Provider, получено: {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_is_typed() {
        let base_url = stub_provider(None).await;
        let history = [Message::user("привет")];
        let err = agent(base_url)
            .ask(&history, &ChatSettings::default())
            .await
            .expect_err("ожидался таймаут");

        assert!(
            matches!(err.downcast_ref::<AgentError>(), Some(AgentError::Timeout)),
            "ожидался Timeout, получено: {err:#}"
        );
    }

    #[tokio::test]
    async fn missing_key_is_typed_and_uses_hint() {
        let agent = HttpAgent::from_config(&Config::default(), Arc::new(ExchangeLog::disabled()))
            .expect("агент")
            .with_missing_key_hint("подсказка вызывающей стороны");
        let history = [Message::user("привет")];
        let err = agent
            .ask(&history, &ChatSettings::default())
            .await
            .expect_err("ожидалась ошибка ключа");

        assert!(matches!(
            err.downcast_ref::<AgentError>(),
            Some(AgentError::MissingApiKey { .. })
        ));
        assert!(format!("{err}").contains("подсказка вызывающей стороны"));
    }
}
