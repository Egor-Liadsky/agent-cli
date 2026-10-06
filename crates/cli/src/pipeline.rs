//! Пайплайн MCP-инструментов `search → summarize → save_to_file` сервера
//! `pipeline-mcp` (<https://github.com/Egor-Liadsky/pipeline-mcp-agent>).
//!
//! Цепочка выполняется двумя способами над одним процессом сервера:
//!
//! - [`run_pipeline`] — детерминированно, командой `agentcli pipeline run`:
//!   клиент сам вызывает три инструмента по порядку и собирает аргументы
//!   каждого шага из JSON-выхода предыдущего;
//! - [`PipelineTools`] — исполнитель для цикла инструментов чата: порядок
//!   вызовов выбирает модель.
//!
//! Сервер без LLM: `summarize` просит модель у клиента через MCP sampling,
//! поэтому клиент объявляет capability `sampling` и отвечает на
//! `sampling/createMessage` моделью текущих настроек ([`Sampler`]). Как и с
//! `git-mcp`, связь только процессом и протоколом, без cargo-зависимости.

// Sampling помечен в rmcp устаревшим (SEP-2577), но это единственный
// способ дать серверу без ключей модель клиента.
#![allow(deprecated)]

use crate::mcp::format_result;
use crate::tool_loop::ToolExecutor;
use agentcore::agent::{Agent, AgentError, Message, ToolCall, ToolSpec};
use agentcore::config::{ChatSettings, Config};
use agentcore::logging::{
    ExchangeLog, RequestLogEntry, ResponseLogEntry, request_id, unix_timestamp,
};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, CreateMessageRequestParams,
    CreateMessageResult, ErrorData, Implementation, SamplingCapability, SamplingMessage,
};
use rmcp::service::{RequestContext, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SERVER_NAME: &str = "pipeline-mcp";
/// Путь к бинарнику сервера — для сборки из исходников и тестов.
pub const SERVER_PROGRAM_ENV: &str = "AGENTCLI_PIPELINE_MCP";

pub const SEARCH: &str = "search";
pub const SUMMARIZE: &str = "summarize";
pub const SAVE_TO_FILE: &str = "save_to_file";

/// Читающие инструменты; всё прочее, включая будущие, — пишущее.
pub const READ_ONLY_TOOLS: [&str; 2] = [SEARCH, SUMMARIZE];

const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Вызов инструмента. Больше, чем у git: `summarize` ждёт ответа модели
/// через sampling, а облачная модель отвечает десятками секунд.
const CALL_TIMEOUT: Duration = Duration::from_secs(180);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LOG_RESULT_CHARS: usize = 4_000;
const CALL_URL: &str = "mcp+stdio://pipeline-mcp/tools/call";
const SAMPLING_URL: &str = "mcp+stdio://pipeline-mcp/sampling/createMessage";

/// Путь из `AGENTCLI_PIPELINE_MCP`, затем рядом с `agentcli`, иначе по
/// `PATH` — то же правило, что у `git-mcp` (`mcp::server_program`).
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

/// Модель, которой клиент отвечает на `sampling/createMessage`.
#[async_trait]
pub trait Sampler: Send + Sync {
    async fn sample(&self, system: Option<&str>, prompt: &str) -> Result<String>;
}

/// Sampling моделью чата: тот же агент и те же настройки, что у хода.
pub struct AgentSampler<A> {
    pub agent: Arc<A>,
    pub settings: ChatSettings,
}

#[async_trait]
impl<A: Agent + Send + Sync + 'static> Sampler for AgentSampler<A> {
    async fn sample(&self, system: Option<&str>, prompt: &str) -> Result<String> {
        // Системный промпт сервера идёт в текст реплики, а не отдельным
        // сообщением: сервис `agentd` принимает в истории только роли
        // user/assistant/tool, а свой системный промпт берёт из настроек чата.
        let text = match system {
            Some(system) => format!("{system}\n\n{prompt}"),
            None => prompt.to_string(),
        };
        Ok(self
            .agent
            .ask(&[Message::user(text)], &self.settings)
            .await?
            .content)
    }
}

/// Обработчик клиентской стороны: объявляет `sampling`, только если есть
/// чем отвечать, — иначе сервер сам перейдёт на экстрактивную сводку.
#[derive(Clone)]
struct Handler {
    sampler: Option<Arc<dyn Sampler>>,
    log: Arc<ExchangeLog>,
}

impl ClientHandler for Handler {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> std::result::Result<CreateMessageResult, ErrorData> {
        let Some(sampler) = &self.sampler else {
            return Err(ErrorData::invalid_request(
                "клиент не поддерживает sampling",
                None,
            ));
        };
        let prompt = params
            .messages
            .into_iter()
            .flat_map(|message| message.content.into_vec())
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        let id = request_id();
        self.log.log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url: SAMPLING_URL,
            model: "",
            request: json!({ "system": params.system_prompt, "prompt": prompt }),
        });
        let started_at = Instant::now();
        let result = sampler
            .sample(params.system_prompt.as_deref(), &prompt)
            .await;
        self.log.log_response(&ResponseLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            status: if result.is_ok() { 200 } else { 500 },
            duration_ms: started_at.elapsed().as_millis(),
            response: match &result {
                Ok(text) => Value::String(text.clone()),
                Err(err) => Value::String(format!("{err:#}")),
            },
        });
        match result {
            Ok(text) => Ok(CreateMessageResult::new(
                SamplingMessage::assistant_text(text),
                "agentcli".to_string(),
            )),
            Err(err) => Err(ErrorData::internal_error(format!("{err:#}"), None)),
        }
    }

    fn get_info(&self) -> ClientConfig {
        let mut capabilities = ClientCapabilities::default();
        if self.sampler.is_some() {
            capabilities.sampling = Some(SamplingCapability::default());
        }
        ClientConfig::new(
            capabilities,
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }
}

/// Процесс `pipeline-mcp` на время одной команды или одного хода: сервер
/// без состояния, держать его между ходами незачем.
pub struct PipelineServer {
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, Handler>>>,
    specs: Vec<ToolSpec>,
    log: Arc<ExchangeLog>,
}

impl PipelineServer {
    pub async fn start(
        program: &Path,
        root: &str,
        output: &str,
        sampler: Option<Arc<dyn Sampler>>,
        log: Arc<ExchangeLog>,
    ) -> Result<Self> {
        let root = expand(root);
        if !root.is_dir() {
            return Err(
                unavailable(format!("каталог поиска {} не существует", root.display())).into(),
            );
        }
        let mut command = tokio::process::Command::new(program);
        command
            .arg("--root")
            .arg(&root)
            .arg("--output")
            .arg(expand(output))
            .kill_on_drop(true);
        // stderr сервера рисовал бы поверх TUI.
        let (transport, _stderr) = TokioChildProcess::builder(command)
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    unavailable(format!(
                        "не найден {}: соберите сервер (cd mcp/pipeline && cargo install --path crates/pipeline) \
                         или укажите путь к бинарнику в {SERVER_PROGRAM_ENV}",
                        program.display()
                    ))
                } else {
                    unavailable(format!("процесс не запустился: {err}"))
                }
            })?;
        let handler = Handler {
            sampler,
            log: log.clone(),
        };
        let (service, tools) = tokio::time::timeout(START_TIMEOUT, async {
            let service = handler
                .serve(transport)
                .await
                .map_err(|err| unavailable(format!("рукопожатие MCP не прошло: {err}")))?;
            let tools = service.peer().list_all_tools().await.map_err(|err| {
                unavailable(format!("не удалось получить список инструментов: {err}"))
            })?;
            Ok::<_, AgentError>((service, tools))
        })
        .await
        .map_err(|_| {
            unavailable(format!(
                "сервер не запустился за {} с",
                START_TIMEOUT.as_secs()
            ))
        })??;
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

    /// Сервер для чата по каталогам из конфига (`config pipeline`).
    pub async fn start_from_config(
        config: &Config,
        sampler: Option<Arc<dyn Sampler>>,
        log: Arc<ExchangeLog>,
    ) -> Result<Self> {
        let root = config.pipeline_root.clone().unwrap_or_default();
        let output = config.effective_pipeline_output().unwrap_or_default();
        Self::start(&server_program(), &root, &output, sampler, log).await
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Сырой результат вызова с записью в журнал обмена. `Err` — сервер
    /// не ответил (упал, завис): это уже не ошибка инструмента.
    async fn call_raw(
        &self,
        name: &str,
        arguments: &Value,
    ) -> std::result::Result<rmcp::model::CallToolResult, AgentError> {
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
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        let outcome = tokio::time::timeout(CALL_TIMEOUT, peer.call_tool(params)).await;
        let (status, logged, result) = match outcome {
            Ok(Ok(result)) => {
                let status = if result.is_error == Some(true) {
                    500
                } else {
                    200
                };
                let text = serde_json::to_string(&result.content).unwrap_or_default();
                (status, text, Ok(result))
            }
            Ok(Err(ServiceError::McpError(error))) => {
                let text = format!("Ошибка инструмента: {}", error.message);
                (
                    500,
                    text.clone(),
                    Ok(rmcp::model::CallToolResult::error(vec![
                        rmcp::model::ContentBlock::text(text),
                    ])),
                )
            }
            Ok(Err(error)) => {
                let text = format!("сервер перестал отвечать: {error}");
                (500, text.clone(), Err(unavailable(text)))
            }
            Err(_) => {
                let text = format!("вызов {name} не уложился в {} с", CALL_TIMEOUT.as_secs());
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

    pub async fn shutdown(&self) {
        if let Some(mut service) = self.service.lock().await.take() {
            let _ = tokio::time::timeout(STOP_TIMEOUT, service.close()).await;
        }
    }
}

pub(crate) fn expand(path: &str) -> PathBuf {
    let trimmed = path.trim();
    match trimmed.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(trimmed)),
        None => PathBuf::from(trimmed),
    }
}

/// Вызов шага пайплайна: структурированный выход или текст ошибки
/// инструмента. Трейт отделяет порядок цепочки от процесса — тест
/// проверяет передачу данных без сервера.
#[async_trait]
pub trait StepCaller: Send + Sync {
    async fn call_step(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<std::result::Result<Value, String>>;
}

#[async_trait]
impl StepCaller for PipelineServer {
    async fn call_step(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<std::result::Result<Value, String>> {
        let result = self.call_raw(name, &arguments).await?;
        let content = serde_json::to_value(&result.content)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
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
}

/// Итог пайплайна: выход каждого шага как есть.
#[derive(Debug, Clone)]
pub struct PipelineReport {
    pub search: Value,
    pub summary: Value,
    pub saved: Value,
}

/// Параметры одного прогона.
pub struct PipelineRequest<'a> {
    pub query: &'a str,
    pub file_name: &'a str,
    pub max_results: Option<u32>,
    pub overwrite: bool,
}

/// Что показывать пользователю по ходу цепочки.
pub trait PipelineObserver {
    fn step(&self, _index: usize, _name: &str) {}
}

impl PipelineObserver for () {}

fn step_failed(name: &str, message: String) -> anyhow::Error {
    anyhow::anyhow!("шаг {name} не выполнен: {message}")
}

/// Цепочка `search → summarize → save_to_file`. Аргументы следующего шага
/// берутся из выхода предыдущего без пересборки (`matches` целиком,
/// `summary` как `content`), а после каждого шага проверяется, что данные
/// дошли: `summarize` получил столько совпадений, сколько нашёл поиск,
/// `save_to_file` записал ровно сводку (сверка SHA-256).
pub async fn run_pipeline(
    caller: &dyn StepCaller,
    request: &PipelineRequest<'_>,
    observer: &dyn PipelineObserver,
) -> Result<PipelineReport> {
    observer.step(1, SEARCH);
    let mut search_args = json!({ "query": request.query });
    if let Some(max) = request.max_results {
        search_args["max_results"] = json!(max);
    }
    let search = caller
        .call_step(SEARCH, search_args)
        .await?
        .map_err(|err| step_failed(SEARCH, err))?;
    let matches = search
        .get("matches")
        .and_then(Value::as_array)
        .ok_or_else(|| step_failed(SEARCH, "в ответе нет matches".into()))?;
    if matches.is_empty() {
        // Сводка по пустому и файл с ней ничего не дают: цепочка
        // останавливается до записи.
        anyhow::bail!(
            "по запросу «{}» ничего не найдено — файл не записан",
            request.query
        );
    }

    observer.step(2, SUMMARIZE);
    let summary = caller
        .call_step(
            SUMMARIZE,
            json!({ "query": search["query"], "matches": search["matches"] }),
        )
        .await?
        .map_err(|err| step_failed(SUMMARIZE, err))?;
    if summary.get("input_matches").and_then(Value::as_u64) != Some(matches.len() as u64) {
        anyhow::bail!(
            "summarize получил {} совпадений вместо {}",
            summary["input_matches"],
            matches.len()
        );
    }
    let text = summary
        .get("summary")
        .and_then(Value::as_str)
        .ok_or_else(|| step_failed(SUMMARIZE, "в ответе нет summary".into()))?;

    observer.step(3, SAVE_TO_FILE);
    let saved = caller
        .call_step(
            SAVE_TO_FILE,
            json!({ "file_name": request.file_name, "content": text, "overwrite": request.overwrite }),
        )
        .await?
        .map_err(|err| step_failed(SAVE_TO_FILE, err))?;
    let expected = sha256_hex(text.as_bytes());
    if saved.get("sha256").and_then(Value::as_str) != Some(expected.as_str()) {
        anyhow::bail!(
            "save_to_file записал не то, что вернул summarize: sha256 {} вместо {expected}",
            saved["sha256"]
        );
    }
    Ok(PipelineReport {
        search,
        summary,
        saved,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Инструменты пайплайна в чате: сервер хода и классификация вызовов.
pub struct PipelineTools {
    pub server: PipelineServer,
}

#[async_trait]
impl ToolExecutor for PipelineTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.server.specs().to_vec()
    }

    fn is_write(&self, name: &str) -> bool {
        !is_read_only(name)
    }

    async fn call(&self, call: &ToolCall) -> Result<String> {
        let result = self.server.call_raw(&call.name, &call.arguments).await?;
        let content = serde_json::to_value(&result.content)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        Ok(format_result(&content, result.is_error == Some(true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Сервер-подделка: запоминает аргументы шагов и отвечает так, как
    /// ответил бы настоящий.
    #[derive(Default)]
    struct FakeSteps {
        calls: Mutex<Vec<(String, Value)>>,
        empty_search: bool,
        corrupt_save: bool,
    }

    #[async_trait]
    impl StepCaller for FakeSteps {
        async fn call_step(
            &self,
            name: &str,
            arguments: Value,
        ) -> Result<std::result::Result<Value, String>> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), arguments.clone()));
            Ok(Ok(match name {
                SEARCH if self.empty_search => {
                    json!({ "query": arguments["query"], "matches": [] })
                }
                SEARCH => json!({
                    "query": arguments["query"],
                    "matches": [
                        { "path": "a.rs", "line": 1, "text": "ToolSet one" },
                        { "path": "b.md", "line": 4, "text": "ToolSet two" }
                    ],
                    "files_scanned": 2,
                    "truncated": false
                }),
                SUMMARIZE => json!({
                    "summary": format!("сводка по {} строкам", arguments["matches"].as_array().unwrap().len()),
                    "method": "sampling",
                    "sources": ["a.rs", "b.md"],
                    "input_matches": arguments["matches"].as_array().unwrap().len()
                }),
                SAVE_TO_FILE => {
                    let content = if self.corrupt_save {
                        "другое"
                    } else {
                        arguments["content"].as_str().unwrap()
                    };
                    json!({ "path": "/out/x.md", "bytes": content.len(), "sha256": sha256_hex(content.as_bytes()) })
                }
                other => panic!("неожиданный инструмент {other}"),
            }))
        }
    }

    fn request() -> PipelineRequest<'static> {
        PipelineRequest {
            query: "ToolSet",
            file_name: "x.md",
            max_results: None,
            overwrite: false,
        }
    }

    #[tokio::test]
    async fn each_step_gets_the_previous_output() {
        let steps = FakeSteps::default();
        let report = run_pipeline(&steps, &request(), &()).await.unwrap();
        let calls = steps.calls.lock().unwrap().clone();
        let names: Vec<&str> = calls.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec![SEARCH, SUMMARIZE, SAVE_TO_FILE]);
        assert_eq!(calls[1].1["matches"], report.search["matches"]);
        assert_eq!(calls[1].1["query"], "ToolSet");
        assert_eq!(calls[2].1["content"], report.summary["summary"]);
        assert_eq!(calls[2].1["file_name"], "x.md");
    }

    #[tokio::test]
    async fn empty_search_stops_before_writing() {
        let steps = FakeSteps {
            empty_search: true,
            ..Default::default()
        };
        let err = run_pipeline(&steps, &request(), &()).await.unwrap_err();
        assert!(err.to_string().contains("ничего не найдено"), "{err}");
        assert_eq!(steps.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn saved_content_is_checked_against_summary() {
        let steps = FakeSteps {
            corrupt_save: true,
            ..Default::default()
        };
        let err = run_pipeline(&steps, &request(), &()).await.unwrap_err();
        assert!(err.to_string().contains("sha256"), "{err}");
    }

    #[test]
    fn only_save_is_a_write() {
        assert!(is_read_only(SEARCH));
        assert!(is_read_only(SUMMARIZE));
        assert!(!is_read_only(SAVE_TO_FILE));
        assert!(!is_read_only("delete_everything"));
    }

    /// Агент, запоминающий историю запроса.
    #[derive(Default)]
    struct RecordingAgent(Mutex<Vec<Message>>);

    #[async_trait]
    impl Agent for RecordingAgent {
        async fn ask(
            &self,
            history: &[Message],
            _settings: &ChatSettings,
        ) -> Result<agentcore::agent::AgentReply> {
            *self.0.lock().unwrap() = history.to_vec();
            anyhow::bail!("не нужен ответ")
        }
    }

    #[tokio::test]
    async fn sampler_sends_one_user_message() {
        // Сервис отклоняет роль system в истории: системный промпт сервера
        // должен уйти текстом единственной реплики пользователя.
        let agent = Arc::new(RecordingAgent::default());
        let sampler = AgentSampler {
            agent: agent.clone(),
            settings: ChatSettings::default(),
        };
        let _ = sampler.sample(Some("SYSTEM"), "PROMPT").await;
        let history = agent.0.lock().unwrap().clone();
        assert_eq!(history.len(), 1);
        assert!(matches!(history[0].role, agentcore::agent::Role::User));
        assert_eq!(history[0].content, "SYSTEM\n\nPROMPT");
    }

    struct EchoSampler;

    #[async_trait]
    impl Sampler for EchoSampler {
        async fn sample(&self, _system: Option<&str>, prompt: &str) -> Result<String> {
            Ok(format!("SAMPLED[{}]", prompt.lines().count()))
        }
    }

    /// Живой прогон против собранного сервера:
    /// `AGENTCLI_PIPELINE_MCP=<абсолютный путь> cargo test -p agentcli live_pipeline -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_pipeline_runs_against_real_server() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("agentcli-pipeline-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("a.md"),
            "ToolSet merges executors\nnoise\nToolSet routes calls\n",
        )
        .unwrap();
        let output = root.join("out");
        let log = Arc::new(ExchangeLog::disabled());
        let server = PipelineServer::start(
            &server_program(),
            &root.to_string_lossy(),
            &output.to_string_lossy(),
            Some(Arc::new(EchoSampler)),
            log,
        )
        .await
        .expect("запуск pipeline-mcp");
        let report = run_pipeline(&server, &request(), &()).await.unwrap();
        server.shutdown().await;
        assert_eq!(report.search["matches"].as_array().unwrap().len(), 2);
        assert_eq!(report.summary["method"], "sampling");
        let summary = report.summary["summary"].as_str().unwrap();
        assert!(summary.starts_with("SAMPLED["), "{summary}");
        assert_eq!(
            std::fs::read_to_string(output.join("x.md")).unwrap(),
            summary
        );
    }
}
