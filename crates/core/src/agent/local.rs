//! Агент локальных моделей: прямой запрос к Ollama на машине пользователя.
//!
//! Локальный путь остаётся в ядре, потому что им пользуются оба потребителя —
//! консольный клиент (как единственным прямым вызовом модели) и сетевой
//! сервис (как одним из провайдеров).

use super::{ollama, system_prompt, Agent, AgentReply, Message, ToolSpec};
use crate::config::{ChatSettings, Config};
use crate::logging::ExchangeLog;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub struct OllamaAgent {
    /// Клиент без прокси из окружения: запрос никуда не уходит с машины.
    client: reqwest::Client,
    base_url: String,
    /// Модель по умолчанию: используется, если у чата нет своей.
    model: String,
    log: Arc<ExchangeLog>,
}

impl OllamaAgent {
    pub fn from_config(config: &Config, log: Arc<ExchangeLog>) -> Result<Self> {
        Ok(Self {
            client: ollama::client(),
            base_url: config.effective_ollama_url(),
            model: config.ollama_model.clone().unwrap_or_default(),
            log,
        })
    }

    /// Таймаут запроса к локальному серверу.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.client = ollama::client_builder().timeout(timeout).build()?;
        Ok(self)
    }

    /// Модель запроса: своя у чата, иначе модель по умолчанию. Встроенной
    /// модели по умолчанию у Ollama нет: набор зависит от того, что скачано.
    fn model_for(&self, settings: &ChatSettings) -> String {
        settings
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| self.model.clone())
    }
}

#[async_trait]
impl Agent for OllamaAgent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply> {
        self.ask_with_tools(history, settings, &[]).await
    }

    async fn ask_with_tools(
        &self,
        history: &[Message],
        settings: &ChatSettings,
        tools: &[ToolSpec],
    ) -> Result<AgentReply> {
        ollama::chat(
            &self.client,
            &self.base_url,
            &self.model_for(settings),
            history,
            settings,
            system_prompt(settings),
            tools,
            &self.log,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provider;
    use crate::logging::ExchangeLog;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Одноразовый сервер: отдаёт ответ Ollama и возвращает первую строку
    /// запроса, чтобы проверить, куда он ушёл.
    async fn stub_ollama() -> (String, tokio::task::JoinHandle<String>) {
        stub_ollama_with(
            "200 OK",
            r#"{"message":{"content":"ответ"},"prompt_eval_count":1,"eval_count":2}"#,
        )
        .await
    }

    async fn stub_ollama_with(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("порт");
        let addr = listener.local_addr().expect("адрес");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("соединение");
            let request = read_request(&mut socket).await;
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
            request
        });
        (format!("http://{addr}"), handle)
    }

    /// Читает запрос целиком: заголовки и тело по `Content-Length`. Тело с
    /// описаниями инструментов не помещается в один `read`.
    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&data);
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|line| {
                        let lower = line.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if data.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&data).to_string()
    }

    fn agent_for(url: &str) -> (OllamaAgent, ChatSettings) {
        let config = Config {
            ollama_url: Some(url.to_string()),
            ollama_model: Some("gemma4:26b".to_string()),
            ..Config::default()
        };
        let agent =
            OllamaAgent::from_config(&config, Arc::new(ExchangeLog::disabled())).expect("агент");
        let settings = ChatSettings {
            provider: Provider::Ollama,
            ..ChatSettings::default()
        };
        (agent, settings)
    }

    fn git_status_spec() -> ToolSpec {
        ToolSpec {
            name: "git_status".into(),
            description: Some("Shows the working tree status".into()),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    #[tokio::test]
    async fn ollama_tool_calls_are_sent_and_parsed() {
        let (url, handle) = stub_ollama_with(
            "200 OK",
            r#"{"message":{"content":"","tool_calls":[{"function":{"name":"git_status","arguments":{}}}]}}"#,
        )
        .await;
        let (agent, settings) = agent_for(&url);
        let reply = agent
            .ask_with_tools(&[Message::user("статус?")], &settings, &[git_status_spec()])
            .await
            .expect("ответ Ollama");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_0");
        assert_eq!(reply.tool_calls[0].name, "git_status");

        let request = handle.await.expect("запрос");
        assert!(request.contains(r#""tools":[{"type":"function""#), "запрос: {request}");
    }

    #[tokio::test]
    async fn ollama_without_tool_support_is_typed_error() {
        let (url, _handle) = stub_ollama_with(
            "400 Bad Request",
            r#"{"error":"registry.ollama.ai/library/gemma4:26b does not support tools"}"#,
        )
        .await;
        let (agent, settings) = agent_for(&url);
        let err = agent
            .ask_with_tools(&[Message::user("статус?")], &settings, &[git_status_spec()])
            .await
            .expect_err("ошибка");
        assert!(matches!(
            err.downcast_ref::<crate::agent::AgentError>(),
            Some(crate::agent::AgentError::ToolsUnsupported { .. })
        ));
    }

    #[tokio::test]
    async fn local_chat_goes_straight_to_ollama() {
        let (url, handle) = stub_ollama().await;
        let config = Config {
            ollama_url: Some(url.clone()),
            ollama_model: Some("gemma4:26b".to_string()),
            ..Config::default()
        };
        let agent =
            OllamaAgent::from_config(&config, Arc::new(ExchangeLog::disabled())).expect("агент");
        let settings = ChatSettings {
            provider: Provider::Ollama,
            ..ChatSettings::default()
        };

        let reply = agent
            .ask(&[Message::user("привет")], &settings)
            .await
            .expect("ответ Ollama");
        assert_eq!(reply.content, "ответ");
        assert_eq!(reply.model.as_deref(), Some("gemma4:26b"));

        let request = handle.await.expect("запрос");
        // Запрос ушёл прямо на локальный адрес и его нативный путь, а не в
        // сервис: локальные модели остаются исключением.
        assert!(request.contains("POST /api/chat"), "запрос: {request}");
        assert!(!request.contains("/v1/chat"), "запрос: {request}");
    }
}
