//! Облачный провайдер: OpenAI-совместимый `POST {base_url}/chat/completions`.
//!
//! Крейт отделён от ядра намеренно: ключ провайдера принадлежит сервису
//! `agentd`, и консольный клиент не должен иметь этот код в своём графе
//! зависимостей. Запрет держится сборкой, а не договорённостью.

use agentcore::agent::{
    close_dangling_tool_calls, system_prompt, transport_error, Agent, AgentError, AgentReply,
    Message, MessageMeta, Role, ToolCall, ToolSpec,
};
use agentcore::config::{ChatSettings, DEFAULT_MODEL};
use agentcore::logging::{
    request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Базовый URL провайдера по умолчанию.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

pub struct UpstreamAgent {
    client: reqwest::Client,
    /// Пустая строка означает «ключ не задан»: агент создаётся и без ключа,
    /// чтобы его можно было ввести уже в настройках чата.
    api_key: String,
    base_url: String,
    /// Модель по умолчанию: используется, если у чата нет своей.
    model: String,
    /// Журнал обмена с провайдером. Назначение задаёт вызывающая сторона.
    log: Arc<ExchangeLog>,
    /// Подсказка вызывающей стороны в сообщении об отсутствующем ключе.
    missing_key_hint: Option<String>,
}

impl UpstreamAgent {
    /// Ключ, адрес провайдера и модель по умолчанию задаёт вызывающая
    /// сторона: ключ принадлежит ей, а не пользовательскому конфигу.
    /// Пустой `base_url` означает [`DEFAULT_BASE_URL`].
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        log: Arc<ExchangeLog>,
    ) -> Self {
        let base_url = base_url.into();
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: if base_url.trim().is_empty() {
                DEFAULT_BASE_URL.to_string()
            } else {
                base_url
            },
            model: model.into(),
            log,
            missing_key_hint: None,
        }
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
        Ok(self)
    }

    /// Модель запроса: своя у чата, иначе модель по умолчанию, иначе
    /// встроенное значение.
    fn model_for(&self, settings: &ChatSettings) -> String {
        settings
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| {
                if self.model.trim().is_empty() {
                    DEFAULT_MODEL.to_string()
                } else {
                    self.model.clone()
                }
            })
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
    /// Описания инструментов. Пустой список не отправляется: не все модели
    /// принимают само поле.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDef<'a>>>,
}

#[derive(Serialize)]
struct ToolDef<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolFunctionDef<'a>,
}

#[derive(Serialize)]
struct ToolFunctionDef<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    parameters: &'a serde_json::Value,
}

/// Вызов инструмента в формате OpenAI: аргументы — строка с JSON.
#[derive(Serialize, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default = "function_kind")]
    kind: String,
    function: WireToolFunction,
}

fn function_kind() -> String {
    "function".to_string()
}

#[derive(Serialize, Deserialize)]
struct WireToolFunction {
    name: String,
    #[serde(default)]
    arguments: String,
}

impl WireToolCall {
    fn from_call(call: &ToolCall) -> Self {
        Self {
            id: call.id.clone(),
            kind: function_kind(),
            function: WireToolFunction {
                name: call.name.clone(),
                arguments: match &call.arguments {
                    // Неразобранные аргументы модели возвращаются как были.
                    serde_json::Value::String(raw) => raw.clone(),
                    other => other.to_string(),
                },
            },
        }
    }

    /// Строка `arguments` разбирается в объект; при ошибке разбора она
    /// сохраняется как `Value::String` — исполнитель вернёт модели ошибку
    /// аргументов, а не уронит ход.
    fn into_call(self, index: usize) -> ToolCall {
        let raw = self.function.arguments;
        let arguments = if raw.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw))
        };
        ToolCall {
            id: if self.id.is_empty() {
                format!("call_{index}")
            } else {
                self.id
            },
            name: self.function.name,
            arguments,
        }
    }
}

#[derive(Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    /// `null` у ответа модели из одних вызовов: так его отдаёт и принимает
    /// OpenAI-формат.
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
    /// Модель, которой провайдер ответил на самом деле.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    /// `null` или отсутствует, если модель ответила одними вызовами.
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
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
    AgentError::provider(status.as_u16(), message)
}

impl UpstreamAgent {
    fn build_messages(&self, history: &[Message], settings: &ChatSettings) -> Vec<ChatMessage> {
        let history = close_dangling_tool_calls(history);
        let (history_system, history) = match history.split_first() {
            Some((first, rest)) if matches!(first.role, Role::System) => {
                (Some(first.content.clone()), rest)
            }
            _ => (None, history.as_slice()),
        };
        let combined_system = match (system_prompt(settings), history_system) {
            (Some(settings), Some(history)) => Some(format!("{settings}\n\n{history}")),
            (Some(settings), None) => Some(settings),
            (None, Some(history)) => Some(history),
            (None, None) => None,
        };
        let mut messages = Vec::with_capacity(history.len() + 1);
        if let Some(content) = combined_system {
            messages.push(ChatMessage {
                role: "system",
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        }
        messages.extend(history.iter().map(|m| ChatMessage {
            role: match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::Tool => "tool",
            },
            content: if m.content.is_empty() && !m.tool_calls.is_empty() {
                None
            } else {
                Some(m.content.clone())
            },
            tool_calls: m.tool_calls.iter().map(WireToolCall::from_call).collect(),
            tool_call_id: m.tool_call_id.clone(),
        }));
        messages
    }

    fn build_request<'a>(
        &self,
        messages: Vec<ChatMessage>,
        settings: &ChatSettings,
        model: &'a str,
        tools: &'a [ToolSpec],
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
            tools: (!tools.is_empty()).then(|| {
                tools
                    .iter()
                    .map(|tool| ToolDef {
                        kind: "function",
                        function: ToolFunctionDef {
                            name: &tool.name,
                            description: tool.description.as_deref(),
                            parameters: &tool.parameters,
                        },
                    })
                    .collect()
            }),
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

    meta.model = parsed.model.clone();
    if let Some(usage) = parsed.usage {
        meta.prompt_tokens = usage.prompt_tokens;
        meta.completion_tokens = usage.completion_tokens;
        meta.total_tokens = usage.total_tokens;
        meta.reasoning_tokens = usage
            .completion_tokens_details
            .and_then(|d| d.reasoning_tokens);
    }

    let model = parsed.model.clone();
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

    let tool_calls = message
        .tool_calls
        .into_iter()
        .enumerate()
        .map(|(index, call)| call.into_call(index))
        .collect();

    Ok(AgentReply {
        content: message.content.unwrap_or_default(),
        reasoning,
        meta,
        model,
        policy: None,
        context: None,
        tool_calls,
    })
}

#[async_trait]
impl Agent for UpstreamAgent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply> {
        self.ask_with_tools(history, settings, &[]).await
    }

    async fn ask_with_tools(
        &self,
        history: &[Message],
        settings: &ChatSettings,
        tools: &[ToolSpec],
    ) -> Result<AgentReply> {
        let messages = self.build_messages(history, settings);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let model = self.model_for(settings);
        let request_body = self.build_request(messages, settings, &model, tools);

        let (body, meta) = self.send_request(&url, &request_body).await?;
        extract_answer(&body, meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn agent(base_url: String) -> UpstreamAgent {
        UpstreamAgent::new(
            "test-key",
            base_url,
            DEFAULT_MODEL,
            Arc::new(ExchangeLog::disabled()),
        )
        .with_request_timeout(Duration::from_millis(300))
        .expect("таймаут")
    }

    #[test]
    fn build_messages_splices_settings_prompt_with_history_system_message() {
        let agent = agent("http://127.0.0.1:0".to_string());
        let history = [Message::system("факты чата: ..."), Message::user("привет")];
        let messages = agent.build_messages(&history, &ChatSettings::default());
        let roles: Vec<&str> = messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec!["system", "user"]);
    }

    #[test]
    fn build_messages_keeps_single_system_message_without_settings_prompt() {
        let agent = agent("http://127.0.0.1:0".to_string());
        let history = [Message::system("базовый текст"), Message::user("привет")];
        let messages = agent.build_messages(&history, &ChatSettings::default());
        let roles: Vec<&str> = messages.iter().map(|m| m.role).collect();
        assert_eq!(roles.iter().filter(|r| **r == "system").count(), 1);
        assert_eq!(roles[0], "system");
        assert_eq!(messages[0].content.as_deref(), Some("базовый текст"));
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
            Some(AgentError::Provider {
                status, message, ..
            }) => {
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
            matches!(
                err.downcast_ref::<AgentError>(),
                Some(AgentError::Timeout { .. })
            ),
            "ожидался Timeout, получено: {err:#}"
        );
    }

    #[tokio::test]
    async fn missing_key_is_typed_and_uses_hint() {
        let agent = UpstreamAgent::new("", "", DEFAULT_MODEL, Arc::new(ExchangeLog::disabled()))
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

    /// Одноразовый сервер, который отдаёт ответ и возвращает тело запроса.
    async fn capturing_provider(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("порт");
        let addr = listener.local_addr().expect("адрес");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("соединение");
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            let request_body = loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                data.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&data).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length || read == 0 {
                        break String::from_utf8_lossy(&data[end + 4..]).to_string();
                    }
                }
                if read == 0 {
                    break String::new();
                }
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
            request_body
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn tool_calls_with_null_content_are_parsed() {
        let (base_url, handle) = capturing_provider(
            r#"{"model":"deepseek-v4-flash","choices":[{"message":{"content":null,"tool_calls":[{"id":"call_abc","type":"function","function":{"name":"git_log","arguments":"{\"max_count\":3}"}}]}}]}"#,
        )
        .await;
        let tools = [ToolSpec {
            name: "git_log".into(),
            description: Some("Shows commit logs".into()),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let history = [
            Message::user("что нового?"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "call_0".into(),
                    name: "git_status".into(),
                    arguments: serde_json::json!({}),
                }],
            ),
            Message::tool_result("call_0", "git_status", "clean"),
        ];
        let reply = agent(base_url)
            .ask_with_tools(&history, &ChatSettings::default(), &tools)
            .await
            .expect("ответ");
        assert_eq!(reply.content, "");
        assert_eq!(
            reply.tool_calls,
            vec![ToolCall {
                id: "call_abc".into(),
                name: "git_log".into(),
                arguments: serde_json::json!({ "max_count": 3 }),
            }]
        );

        let request: serde_json::Value =
            serde_json::from_str(&handle.await.expect("запрос")).expect("JSON запроса");
        assert_eq!(request["tools"][0]["type"], "function");
        assert_eq!(request["tools"][0]["function"]["name"], "git_log");
        assert_eq!(request["messages"][1]["content"], serde_json::Value::Null);
        assert_eq!(request["messages"][1]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(request["messages"][2]["role"], "tool");
        assert_eq!(request["messages"][2]["tool_call_id"], "call_0");
    }

    #[test]
    fn unparsable_arguments_are_kept_as_string() {
        let call = WireToolCall {
            id: String::new(),
            kind: function_kind(),
            function: WireToolFunction {
                name: "git_add".into(),
                arguments: "{не json".into(),
            },
        }
        .into_call(2);
        assert_eq!(call.id, "call_2");
        assert_eq!(call.arguments, serde_json::Value::String("{не json".into()));
    }

    #[test]
    fn request_without_tools_has_no_tools_field() {
        let agent = agent("http://127.0.0.1:0".to_string());
        let settings = ChatSettings::default();
        let messages = agent.build_messages(&[Message::user("привет")], &settings);
        let request = agent.build_request(messages, &settings, "m", &[]);
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("tools").is_none());
        assert!(value["messages"][0].get("tool_calls").is_none());
    }
}
