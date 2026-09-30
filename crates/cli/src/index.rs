//! Индекс документов: клиент MCP-сервера `index-mcp`
//! (<https://github.com/Egor-Liadsky/index-mcp-agent>).
//!
//! Как и `pipeline-mcp`, сервер — процесс клиента на время одной команды
//! или одного хода, связь только JSON-RPC через stdin/stdout, без
//! cargo-зависимости. Настройки — поля `index_*` в `Config` ядра
//! (`agentcli config index`, раздел «Индекс документов» в `Ctrl+P`): индекс
//! один на машину, а не свойство чата.
//!
//! Сервер получает из конфига то, что задаётся при запуске (база, модель,
//! адрес Ollama, стратегия поиска), а параметры сборки — аргументами
//! `index_build`. Модель чата видит `index_search`, `index_status` и
//! `index_build`; `index_models` и `index_compare` — для команд и настроек,
//! в чат они не попадают.

use crate::mcp::format_result;
use crate::pipeline::expand;
use crate::tool_loop::ToolExecutor;
use agentcore::agent::{AgentError, ToolCall, ToolSpec};
use agentcore::config::Config;
use agentcore::logging::{request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

pub const SERVER_NAME: &str = "index-mcp";
/// Путь к бинарнику сервера — для сборки из исходников и тестов.
pub const SERVER_PROGRAM_ENV: &str = "AGENTCLI_INDEX_MCP";

pub const INDEX_SEARCH: &str = "index_search";
pub const INDEX_STATUS: &str = "index_status";
pub const INDEX_MODELS: &str = "index_models";
pub const INDEX_BUILD: &str = "index_build";

/// Читающие инструменты; всё прочее, включая будущие, — пишущее.
pub const READ_ONLY_TOOLS: [&str; 3] = [INDEX_SEARCH, INDEX_STATUS, INDEX_MODELS];
/// Что сервер отдаёт модели чата. `index_models` и `index_compare` нужны
/// человеку (настройки, команды), а не разговору.
pub const CHAT_TOOLS: [&str; 3] = [INDEX_SEARCH, INDEX_STATUS, INDEX_BUILD];

const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Запрос к Ollama на холодной модели: загрузка весов занимает десятки секунд.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Сборка на большом корпусе — минуты эмбеддинга.
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LOG_RESULT_CHARS: usize = 4_000;
const CALL_URL: &str = "mcp+stdio://index-mcp/tools/call";

/// Путь из `AGENTCLI_INDEX_MCP`, затем рядом с `agentcli`, иначе по `PATH` —
/// то же правило, что у `git-mcp` и `pipeline-mcp`.
pub fn server_program() -> PathBuf {
    if let Some(path) = std::env::var_os(SERVER_PROGRAM_ENV).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let name = format!("{SERVER_NAME}{}", std::env::consts::EXE_SUFFIX);
    if let Some(beside) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
        .map(|dir| dir.join(&name))
        && beside.is_file()
    {
        return beside;
    }
    PathBuf::from(name)
}

pub fn is_read_only(name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&name)
}

fn unavailable(reason: impl Into<String>) -> AgentError {
    AgentError::ToolServerUnavailable {
        server: SERVER_NAME.to_string(),
        reason: reason.into(),
    }
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|value| !value.is_empty())
}

/// Настройки индекса, снятые с конфига: их можно поправить флагами команды,
/// не трогая сам конфиг.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexSettings {
    pub root: Option<String>,
    pub db: String,
    pub strategy: Option<String>,
    pub model: Option<String>,
    pub unit: Option<String>,
    pub chunk_size: Option<usize>,
    pub overlap: Option<usize>,
    pub max_section: Option<usize>,
    pub min_section: Option<usize>,
    pub ollama_url: Option<String>,
}

impl IndexSettings {
    /// `Err`, если база не задана: без неё нечего открывать и некуда строить.
    pub fn from_config(config: &Config) -> Result<Self> {
        let db = non_empty(&config.index_db)
            .ok_or_else(|| anyhow::anyhow!("не задана база индекса: agentcli config index set --db <ФАЙЛ>"))?
            .to_string();
        Ok(Self {
            root: non_empty(&config.index_root).map(str::to_string),
            db,
            strategy: non_empty(&config.index_strategy).map(str::to_string),
            model: non_empty(&config.index_model).map(str::to_string),
            unit: non_empty(&config.index_unit).map(str::to_string),
            chunk_size: config.index_chunk_size,
            overlap: config.index_overlap,
            max_section: config.index_max_section,
            min_section: config.index_min_section,
            ollama_url: non_empty(&config.index_ollama_url).map(str::to_string),
        })
    }

    /// Аргументы `index-mcp serve`. Модель и адрес Ollama идут флагами
    /// запуска, а не аргументами инструментов: так поиск и сборка в одном
    /// процессе работают одной моделью. Стратегия `all` поиску не подходит —
    /// её решает сервер (единственная в базе) или сама модель аргументом.
    pub fn serve_args(&self) -> Vec<String> {
        let mut args = vec!["serve".to_string(), "--db".to_string(), expand(&self.db).to_string_lossy().into_owned()];
        if let Some(strategy) = self.strategy.as_deref().filter(|strategy| *strategy != "all") {
            args.extend(["--strategy".to_string(), strategy.to_string()]);
        }
        if let Some(model) = &self.model {
            args.extend(["--model".to_string(), model.clone()]);
        }
        if let Some(url) = &self.ollama_url {
            args.extend(["--ollama-url".to_string(), url.clone()]);
        }
        args
    }

    /// Аргументы `index_build` из настроек; чего нет — берёт умолчание
    /// `index-mcp build`. `None`, пока не задан каталог с `.docx`.
    pub fn build_arguments(&self) -> Option<Value> {
        let root = self.root.as_deref()?;
        let mut args = json!({ "input": expand(root).to_string_lossy() });
        let object = args.as_object_mut()?;
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                object.insert(key.to_string(), value);
            }
        };
        put("strategy", self.strategy.as_ref().map(|value| json!(value)));
        put("unit", self.unit.as_ref().map(|value| json!(value)));
        put("chunk_size", self.chunk_size.map(|value| json!(value)));
        put("overlap", self.overlap.map(|value| json!(value)));
        put("max_section", self.max_section.map(|value| json!(value)));
        put("min_section", self.min_section.map(|value| json!(value)));
        Some(args)
    }
}

/// Куда девать stderr сервера: там идёт ход сборки.
#[derive(Clone)]
pub enum Progress {
    /// TUI: stderr рисовал бы поверх экрана.
    Discard,
    /// Команда CLI: ход сборки виден в терминале.
    Terminal,
    /// Строки для своего вывода (строка состояния TUI).
    Lines(Arc<dyn Fn(String) + Send + Sync>),
}

/// Процесс `index-mcp serve` на время команды или хода.
pub struct IndexServer {
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, ()>>>,
    specs: Vec<ToolSpec>,
    log: Arc<ExchangeLog>,
}

impl IndexServer {
    pub async fn start(program: &Path, settings: &IndexSettings, progress: Progress, log: Arc<ExchangeLog>) -> Result<Self> {
        let mut command = tokio::process::Command::new(program);
        command.args(settings.serve_args()).kill_on_drop(true);
        let stderr = match progress {
            Progress::Discard => Stdio::null(),
            Progress::Terminal => Stdio::inherit(),
            Progress::Lines(_) => Stdio::piped(),
        };
        let (transport, piped) = TokioChildProcess::builder(command).stderr(stderr).spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                unavailable(format!(
                    "не найден {}: соберите сервер (cd mcp/index && cargo install --path crates/index) \
                     или укажите путь к бинарнику в {SERVER_PROGRAM_ENV}",
                    program.display()
                ))
            } else {
                unavailable(format!("процесс не запустился: {err}"))
            }
        })?;
        if let (Progress::Lines(sink), Some(stderr)) = (progress, piped) {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    sink(line);
                }
            });
        }
        let (service, tools) = tokio::time::timeout(START_TIMEOUT, async {
            let service = ()
                .serve(transport)
                .await
                .map_err(|err| unavailable(format!("рукопожатие MCP не прошло: {err}")))?;
            let tools = service
                .peer()
                .list_all_tools()
                .await
                .map_err(|err| unavailable(format!("не удалось получить список инструментов: {err}")))?;
            Ok::<_, AgentError>((service, tools))
        })
        .await
        .map_err(|_| unavailable(format!("сервер не запустился за {} с", START_TIMEOUT.as_secs())))??;
        let specs = tools
            .into_iter()
            .map(|tool| ToolSpec {
                name: tool.name.to_string(),
                description: tool.description.map(|d| d.to_string()),
                parameters: Value::Object((*tool.input_schema).clone()),
            })
            .collect();
        Ok(Self {
            service: tokio::sync::Mutex::new(Some(service)),
            specs,
            log,
        })
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Сырой результат вызова с записью в журнал обмена. `Err` — сервер не
    /// ответил (упал, завис): это уже не ошибка инструмента.
    async fn call_raw(&self, name: &str, arguments: &Value) -> std::result::Result<CallToolResult, AgentError> {
        let arguments = arguments.as_object().cloned().unwrap_or_default();
        let id = request_id();
        self.log.log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url: CALL_URL,
            model: name,
            request: json!({ "name": name, "arguments": arguments }),
        });
        let started_at = Instant::now();
        let peer = match self.service.lock().await.as_ref() {
            Some(service) => service.peer().clone(),
            None => return Err(unavailable("сервер уже остановлен")),
        };
        let timeout = if name == INDEX_BUILD { BUILD_TIMEOUT } else { CALL_TIMEOUT };
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        let outcome = tokio::time::timeout(timeout, peer.call_tool(params)).await;
        let (status, logged, result) = match outcome {
            Ok(Ok(result)) => {
                let status = if result.is_error == Some(true) { 500 } else { 200 };
                let text = serde_json::to_string(&result.content).unwrap_or_default();
                (status, text, Ok(result))
            }
            Ok(Err(ServiceError::McpError(error))) => {
                let text = format!("Ошибка инструмента: {}", error.message);
                (500, text.clone(), Ok(CallToolResult::error(vec![ContentBlock::text(text)])))
            }
            Ok(Err(error)) => {
                let text = format!("сервер перестал отвечать: {error}");
                (500, text.clone(), Err(unavailable(text)))
            }
            Err(_) => {
                let text = format!("вызов {name} не уложился в {} с", timeout.as_secs());
                (504, text.clone(), Err(unavailable(text)))
            }
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

    /// Вызов инструмента как данных: JSON из `structuredContent` либо текст
    /// ошибки инструмента. Внешний `Err` — сервер недоступен.
    pub async fn call_json(&self, name: &str, arguments: Value) -> Result<std::result::Result<Value, String>> {
        let result = self.call_raw(name, &arguments).await?;
        let content = content_array(&result);
        if result.is_error == Some(true) {
            return Ok(Err(format_result(&content, false)));
        }
        // Сервер дублирует JSON текстом: если `structuredContent` потерялся
        // по дороге, данные берутся из текста.
        let value = result
            .structured_content
            .or_else(|| serde_json::from_str(&format_result(&content, false)).ok());
        Ok(value.ok_or_else(|| format!("{name} вернул не JSON")))
    }

    pub async fn shutdown(&self) {
        if let Some(mut service) = self.service.lock().await.take() {
            let _ = tokio::time::timeout(STOP_TIMEOUT, service.close()).await;
        }
    }
}

fn content_array(result: &CallToolResult) -> Vec<Value> {
    serde_json::to_value(&result.content)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

/// Итог `index_build` одной строкой — для строки состояния TUI.
pub fn build_summary(value: &Value) -> String {
    let strategies: Vec<String> = value["strategies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| format!("{} — {} чанков", s["strategy"].as_str().unwrap_or("?"), s["chunks"]))
        .collect();
    format!(
        "готово: {} (модель {}, dim {})",
        strategies.join(", "),
        value["model"].as_str().unwrap_or("?"),
        value["dim"]
    )
}

/// Инструменты индекса в чате: сервер хода и классификация вызовов.
pub struct IndexTools {
    pub server: IndexServer,
    /// Настройки сборки из конфига: подставляются в `index_build`, чего
    /// модель не назвала сама.
    build_defaults: Option<Value>,
}

impl IndexTools {
    pub async fn start(settings: &IndexSettings, progress: Progress, log: Arc<ExchangeLog>) -> Result<Self> {
        let server = IndexServer::start(&server_program(), settings, progress, log).await?;
        Ok(Self {
            server,
            build_defaults: settings.build_arguments(),
        })
    }
}

/// Аргументы модели поверх настроек конфига: явное слово модели весомее.
fn merge_build_arguments(defaults: Option<&Value>, given: &Value) -> Value {
    let mut merged = defaults.and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(given) = given.as_object() {
        merged.extend(given.clone());
    }
    Value::Object(merged)
}

#[async_trait]
impl ToolExecutor for IndexTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.server
            .specs()
            .iter()
            .filter(|spec| CHAT_TOOLS.contains(&spec.name.as_str()))
            .cloned()
            .map(|mut spec| {
                // Каталог задан в настройках: модели не нужно (и незачем)
                // его знать, а без `input` в `required` она не будет выдумывать путь.
                if spec.name == INDEX_BUILD
                    && self.build_defaults.is_some()
                    && let Some(required) = spec.parameters.get_mut("required").and_then(Value::as_array_mut)
                {
                    required.retain(|name| name != "input");
                }
                spec
            })
            .collect()
    }

    fn is_write(&self, name: &str) -> bool {
        !is_read_only(name)
    }

    async fn call(&self, call: &ToolCall) -> Result<String> {
        let arguments = if call.name == INDEX_BUILD {
            merge_build_arguments(self.build_defaults.as_ref(), &call.arguments)
        } else {
            call.arguments.clone()
        };
        let result = self.server.call_raw(&call.name, &arguments).await?;
        Ok(format_result(&content_array(&result), result.is_error == Some(true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> IndexSettings {
        IndexSettings {
            root: Some("/notes".into()),
            db: "/tmp/idx.db".into(),
            strategy: Some("structure".into()),
            model: Some("bge-m3".into()),
            unit: Some("tokens".into()),
            chunk_size: Some(400),
            overlap: None,
            max_section: Some(600),
            min_section: None,
            ollama_url: Some("http://gpu:11434".into()),
        }
    }

    #[test]
    fn serve_args_carry_db_model_and_url() {
        assert_eq!(
            settings().serve_args(),
            ["serve", "--db", "/tmp/idx.db", "--strategy", "structure", "--model", "bge-m3", "--ollama-url", "http://gpu:11434"]
        );
        // Пустые настройки — сервер берёт свои умолчания; `all` поиску не нужна.
        let bare = IndexSettings { db: "/i.db".into(), strategy: Some("all".into()), ..IndexSettings::default() };
        assert_eq!(bare.serve_args(), ["serve", "--db", "/i.db"]);
    }

    #[test]
    fn build_arguments_hold_only_what_is_set() {
        assert_eq!(
            settings().build_arguments().unwrap(),
            json!({ "input": "/notes", "strategy": "structure", "unit": "tokens", "chunk_size": 400, "max_section": 600 })
        );
        let no_root = IndexSettings { db: "/i.db".into(), ..IndexSettings::default() };
        assert_eq!(no_root.build_arguments(), None);
    }

    #[test]
    fn settings_need_a_database_and_ignore_blank_fields() {
        assert!(IndexSettings::from_config(&Config::default()).is_err());
        let config = Config {
            index_db: Some("/i.db".into()),
            index_root: Some("  ".into()),
            index_model: Some("".into()),
            index_overlap: Some(50),
            ..Config::default()
        };
        let settings = IndexSettings::from_config(&config).unwrap();
        assert_eq!(settings.root, None);
        assert_eq!(settings.model, None);
        assert_eq!(settings.overlap, Some(50));
    }

    #[test]
    fn model_arguments_win_over_config_defaults() {
        let defaults = settings().build_arguments();
        let merged = merge_build_arguments(defaults.as_ref(), &json!({ "strategy": "fixed", "min_chars": 0 }));
        assert_eq!(merged["strategy"], "fixed");
        assert_eq!(merged["input"], "/notes");
        assert_eq!(merged["chunk_size"], 400);
        assert_eq!(merged["min_chars"], 0);
        assert_eq!(merge_build_arguments(None, &json!({ "input": "/x" })), json!({ "input": "/x" }));
    }

    #[test]
    fn only_build_is_a_write_and_chat_hides_models() {
        assert!(is_read_only(INDEX_SEARCH));
        assert!(is_read_only(INDEX_STATUS));
        assert!(!is_read_only(INDEX_BUILD));
        assert!(!is_read_only("index_compare"));
        assert!(!is_read_only("delete_everything"));
        assert!(!CHAT_TOOLS.contains(&INDEX_MODELS));
        assert!(!CHAT_TOOLS.contains(&"index_compare"));
    }

    /// Живой прогон против собранного сервера и настоящего Ollama с
    /// `nomic-embed-text`:
    /// `AGENTCLI_INDEX_MCP=<абсолютный путь> cargo test -p agentcli live_index -- --ignored`.
    /// Адрес Ollama — `INDEX_OLLAMA_URL` (по умолчанию `http://localhost:11434`).
    #[tokio::test]
    #[ignore]
    async fn live_index_builds_and_searches_against_real_server() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("agentcli-index-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = IndexSettings {
            db: dir.join("idx.db").to_string_lossy().into_owned(),
            ollama_url: std::env::var("INDEX_OLLAMA_URL").ok(),
            ..IndexSettings::default()
        };
        let server = IndexServer::start(&server_program(), &settings, Progress::Discard, Arc::new(ExchangeLog::disabled()))
            .await
            .expect("запуск index-mcp");
        let names: Vec<&str> = server.specs().iter().map(|spec| spec.name.as_str()).collect();
        for tool in [INDEX_SEARCH, INDEX_STATUS, INDEX_MODELS, INDEX_BUILD] {
            assert!(names.contains(&tool), "{names:?}");
        }
        let status = server.call_json(INDEX_STATUS, json!({})).await.unwrap().unwrap();
        assert_eq!(status["exists"], false);
        let models = server.call_json(INDEX_MODELS, json!({})).await.unwrap().unwrap();
        assert!(
            models["models"].as_array().unwrap().iter().any(|m| m["name"].as_str().unwrap().starts_with("nomic-embed-text")),
            "{models}"
        );
        // Корпус без .docx строить нечем: сервер отвечает ошибкой инструмента, а не падает.
        let err = server.call_json(INDEX_BUILD, json!({ "input": dir })).await.unwrap().unwrap_err();
        assert!(err.contains("нет файлов .docx"), "{err}");
        server.shutdown().await;
    }
}
