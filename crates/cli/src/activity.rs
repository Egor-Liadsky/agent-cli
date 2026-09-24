//! Сводки активности проектов через MCP: демон `activity-mcp`, его сводки
//! и читающие инструменты для модели.
//!
//! Сервер — отдельный проект (<https://github.com/Egor-Liadsky/activity-mcp-agent>),
//! связанный с клиентом только протоколом. В отличие от `git-mcp`, клиент
//! его не запускает: демон работает постоянно (launchd/systemd), следит за
//! каталогом проектов и сам по расписанию собирает сводки, даже когда
//! `agentcli` не открыт. Поэтому транспорт — Streamable HTTP, а не stdio, и
//! соединение открывается на одну операцию: демон перезапускается
//! независимо, и долгоживущая сессия пережила бы его только переподключением.

use crate::mcp::{format_result, ServerTool};
use crate::tool_loop::ToolExecutor;
use agentcore::agent::{AgentError, ToolCall, ToolSpec};
use agentcore::config::Config;
use agentcore::logging::{request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SERVER_NAME: &str = "activity-mcp";

/// Инструменты, которые получает модель в чате: только читающие.
/// `activity_ack` и `activity_build_digest` меняют состояние демона
/// (прочитанность, границы периодов сводок) — это решение человека, а не
/// модели, поэтому модели они не показываются вовсе.
pub const CHAT_TOOLS: [&str; 3] = ["activity_digest", "activity_projects", "activity_changes"];

/// Демон локальный: если он не ответил за это время, его нет.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = crate::mcp::CALL_TIMEOUT;
const MAX_LOG_RESULT_CHARS: usize = 4_000;

fn unavailable(reason: impl Into<String>) -> AgentError {
    AgentError::ToolServerUnavailable {
        server: SERVER_NAME.to_string(),
        reason: reason.into(),
    }
}

/// Куда и с каким токеном подключаться.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub url: String,
    pub token: Option<String>,
}

impl Endpoint {
    pub fn from_config(config: &Config) -> Self {
        Self {
            url: config.effective_activity_url(),
            token: config.activity_token.clone().filter(|token| !token.trim().is_empty()),
        }
    }
}

/// Сводка в том виде, в каком её отдаёт `activity_digest`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Digest {
    pub id: i64,
    pub period_from: String,
    pub period_to: String,
    #[serde(default)]
    pub acked: bool,
    pub text: String,
}

#[derive(Deserialize)]
struct DigestList {
    digests: Vec<Digest>,
}

#[derive(Deserialize)]
struct BuiltDigest {
    digest: Option<Digest>,
}

/// Текстовые элементы результата как есть, без префиксов и обрезки: их
/// разбирает клиент, а не читает модель.
fn raw_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Соединение с демоном на одну операцию или один ход.
pub struct ActivityClient {
    service: RunningService<RoleClient, ()>,
    url: String,
    log: Arc<ExchangeLog>,
}

impl ActivityClient {
    pub async fn connect(endpoint: &Endpoint, log: Arc<ExchangeLog>) -> std::result::Result<Self, AgentError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint.url.clone());
        if let Some(token) = &endpoint.token {
            config = config.auth_header(token.clone());
        }
        let transport = StreamableHttpClientTransport::from_config(config);
        let service = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| {
                unavailable(format!(
                    "демон не ответил за {} с по адресу {}: запущен ли activity-mcp?",
                    CONNECT_TIMEOUT.as_secs(),
                    endpoint.url
                ))
            })?
            .map_err(|err| unavailable(format!("нет связи с {}: {err}", endpoint.url)))?;
        Ok(Self {
            service,
            url: endpoint.url.clone(),
            log,
        })
    }

    pub async fn tools(&self) -> std::result::Result<Vec<ServerTool>, AgentError> {
        let tools = tokio::time::timeout(CALL_TIMEOUT, self.service.peer().list_all_tools())
            .await
            .map_err(|_| unavailable("tools/list не уложился в срок"))?
            .map_err(|err| unavailable(format!("не удалось получить список инструментов: {err}")))?;
        Ok(tools
            .into_iter()
            .map(|tool| ServerTool {
                name: tool.name.to_string(),
                description: tool.description.map(|d| d.to_string()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect())
    }

    /// Вызов инструмента: элементы `content` и признак ошибки инструмента.
    /// `Err` — только если демон недоступен или протокол сломался.
    pub async fn call(&self, name: &str, arguments: &Value) -> std::result::Result<(Vec<Value>, bool), AgentError> {
        let id = request_id();
        let url = format!("{}#tools/call", self.url);
        self.log.log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url: &url,
            model: name,
            request: json!({ "name": name, "arguments": arguments }),
        });
        let started_at = Instant::now();
        let params = CallToolRequestParams::new(name.to_string())
            .with_arguments(arguments.as_object().cloned().unwrap_or_default());
        let outcome = tokio::time::timeout(CALL_TIMEOUT, self.service.peer().call_tool(params)).await;
        let result = match outcome {
            Ok(Ok(result)) => {
                let content = serde_json::to_value(&result.content)
                    .ok()
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default();
                Ok((content, result.is_error == Some(true)))
            }
            // Неизвестный инструмент, неверные аргументы: ответ есть, это
            // ошибка вызова, а не недоступность демона.
            Ok(Err(ServiceError::McpError(error))) => {
                Ok((vec![json!({ "type": "text", "text": error.message })], true))
            }
            Ok(Err(error)) => Err(unavailable(format!("демон перестал отвечать: {error}"))),
            Err(_) => Err(unavailable(format!("вызов {name} не уложился в {} с", CALL_TIMEOUT.as_secs()))),
        };
        let (status, logged) = match &result {
            Ok((content, is_error)) => (if *is_error { 500 } else { 200 }, raw_text(content)),
            Err(err) => (503, err.to_string()),
        };
        self.log.log_response(&ResponseLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            status,
            duration_ms: started_at.elapsed().as_millis(),
            response: Value::String(logged.chars().take(MAX_LOG_RESULT_CHARS).collect()),
        });
        result
    }

    /// Вызов, ответ которого клиент разбирает сам: ошибка инструмента здесь
    /// — ошибка операции.
    async fn call_json<T: serde::de::DeserializeOwned>(&self, name: &str, arguments: Value) -> Result<T> {
        let (content, is_error) = self.call(name, &arguments).await?;
        let text = raw_text(&content);
        if is_error {
            anyhow::bail!("{name}: {text}");
        }
        serde_json::from_str(&text).map_err(|err| anyhow::anyhow!("{name}: ответ не разобран ({err}): {text}"))
    }

    /// Сводки, новые первыми.
    pub async fn digests(&self, unread_only: bool, limit: u32) -> Result<Vec<Digest>> {
        let list: DigestList = self
            .call_json("activity_digest", json!({ "unread_only": unread_only, "limit": limit }))
            .await?;
        Ok(list.digests)
    }

    pub async fn ack(&self, id: i64) -> Result<()> {
        let (content, is_error) = self.call("activity_ack", &json!({ "id": id })).await?;
        if is_error {
            anyhow::bail!("activity_ack: {}", raw_text(&content));
        }
        Ok(())
    }

    /// Внеплановая сводка; `None` — с прошлой сводки ничего не произошло.
    pub async fn build_digest(&self) -> Result<Option<Digest>> {
        let built: BuiltDigest = self.call_json("activity_build_digest", json!({})).await?;
        Ok(built.digest)
    }

    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.service.close()).await;
    }
}

/// Непрочитанные сводки, старые первыми — в порядке показа.
pub async fn fetch_unread(endpoint: &Endpoint, log: Arc<ExchangeLog>) -> Result<Vec<Digest>> {
    let client = ActivityClient::connect(endpoint, log).await?;
    let result = client.digests(true, 20).await;
    client.close().await;
    let mut digests = result?;
    digests.reverse();
    Ok(digests)
}

/// Отметка сводки прочитанной отдельным соединением.
pub async fn ack(endpoint: &Endpoint, log: Arc<ExchangeLog>, id: i64) -> Result<()> {
    let client = ActivityClient::connect(endpoint, log).await?;
    let result = client.ack(id).await;
    client.close().await;
    result
}

/// Сообщение модели с просьбой пересказать сводку: так пересказ идёт
/// обычной репликой чата и остаётся в его истории, а у демона не нужно ни
/// ключей провайдера, ни своей модели.
pub fn retell_prompt(digest: &Digest) -> String {
    format!(
        "Вот сводка активности моих проектов за период {} — {}. Перескажи главное: \
         над чем шла работа, что заметно изменилось, что осталось незакоммиченным \
         и на что стоит обратить внимание.\n\n{}",
        digest.period_from, digest.period_to, digest.text
    )
}

/// Читающие инструменты демона для модели на время одного хода.
pub struct ActivityTools {
    client: ActivityClient,
    tools: Vec<ServerTool>,
}

impl ActivityTools {
    pub async fn connect(endpoint: &Endpoint, log: Arc<ExchangeLog>) -> std::result::Result<Self, AgentError> {
        let client = ActivityClient::connect(endpoint, log).await?;
        let tools = client.tools().await?;
        Ok(Self { client, tools })
    }

    pub async fn close(self) {
        self.client.close().await;
    }
}

#[async_trait]
impl ToolExecutor for ActivityTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|tool| CHAT_TOOLS.contains(&tool.name.as_str()))
            .map(|tool| ToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            })
            .collect()
    }

    /// Всё, кроме `CHAT_TOOLS`, считается пишущим — на случай, если такой
    /// вызов всё же дойдёт до исполнителя.
    fn is_write(&self, name: &str) -> bool {
        !CHAT_TOOLS.contains(&name)
    }

    async fn call(&self, call: &ToolCall) -> Result<String> {
        let (content, is_error) = self.client.call(&call.name, &call.arguments).await?;
        Ok(format_result(&content, is_error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_uses_defaults_and_drops_empty_token() {
        let config = Config {
            activity_token: Some("  ".into()),
            ..Config::default()
        };
        let endpoint = Endpoint::from_config(&config);
        assert_eq!(endpoint.url, agentcore::config::DEFAULT_ACTIVITY_URL);
        assert_eq!(endpoint.token, None);
    }

    #[test]
    fn digest_list_is_parsed() {
        let text = json!({ "digests": [ { "id": 3, "period_from": "a", "period_to": "b",
                        "created_at": "b", "acked": false, "text": "## Активность" } ] })
        .to_string();
        let list: DigestList = serde_json::from_str(&text).unwrap();
        assert_eq!(list.digests[0].id, 3);
        assert!(retell_prompt(&list.digests[0]).ends_with("## Активность"));
        let built: BuiltDigest = serde_json::from_str(r#"{ "digest": null, "note": "x" }"#).unwrap();
        assert!(built.digest.is_none());
    }

    #[test]
    fn only_read_tools_reach_the_model() {
        assert!(CHAT_TOOLS.contains(&"activity_changes"));
        assert!(!CHAT_TOOLS.contains(&"activity_ack"));
        assert!(!CHAT_TOOLS.contains(&"activity_build_digest"));
    }

    #[tokio::test]
    async fn unreachable_daemon_is_server_unavailable() {
        let endpoint = Endpoint {
            // Порт 9 (discard) на loopback закрыт: соединение отклоняется.
            url: "http://127.0.0.1:9/mcp".into(),
            token: None,
        };
        let err = ActivityClient::connect(&endpoint, Arc::new(ExchangeLog::disabled()))
            .await
            .err()
            .expect("ошибка подключения");
        match err {
            AgentError::ToolServerUnavailable { server, .. } => assert_eq!(server, SERVER_NAME),
            other => panic!("ожидался ToolServerUnavailable, получено: {other:?}"),
        }
    }

    /// Живой тест против запущенного демона: адрес — из
    /// `AGENTCLI_ACTIVITY_URL` или по умолчанию.
    #[tokio::test]
    #[ignore]
    async fn live_daemon_lists_tools_and_digests() {
        let endpoint = Endpoint {
            url: std::env::var("AGENTCLI_ACTIVITY_URL").unwrap_or_else(|_| agentcore::config::DEFAULT_ACTIVITY_URL.into()),
            token: std::env::var("AGENTCLI_ACTIVITY_TOKEN").ok(),
        };
        let tools = ActivityTools::connect(&endpoint, Arc::new(ExchangeLog::disabled()))
            .await
            .expect("подключение к демону");
        let names: Vec<String> = tools.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names.len(), CHAT_TOOLS.len(), "инструменты: {names:?}");
        tools.close().await;
        fetch_unread(&endpoint, Arc::new(ExchangeLog::disabled()))
            .await
            .expect("activity_digest");
    }
}
