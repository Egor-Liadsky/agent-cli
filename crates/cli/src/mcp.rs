//! Git-инструменты через MCP: процесс `agentcli-git-mcp`, его инструменты и
//! вызовы.
//!
//! Сервер — отдельный бинарник этого workspace (`crates/git-mcp`) на Rust,
//! без Python и `uv`: инструменты вызывают системный `git`. Имена и
//! аргументы инструментов совпадают с `mcp-server-git`.
//!
//! Сервер запускается лениво — при первой реплике чата с включёнными
//! инструментами, а не при старте TUI: большинству чатов git не нужен.
//! Один процесс обслуживает один репозиторий; чаты с тем же репозиторием
//! делят его.

use crate::tool_loop::ToolExecutor;
use agentcore::agent::{AgentError, ToolCall, ToolSpec};
use agentcore::logging::{request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const SERVER_NAME: &str = "agentcli-git-mcp";

/// Инструменты, которые только читают репозиторий. Всё прочее — пишущее,
/// включая инструменты будущих версий сервера: неизвестное считается
/// опасным, а не безопасным. Аннотациям сервера (`readOnlyHint`) не
/// доверяем — спецификация MCP прямо называет их недоверенными.
pub const READ_ONLY_TOOLS: [&str; 7] = [
    "git_status",
    "git_diff_unstaged",
    "git_diff_staged",
    "git_diff",
    "git_log",
    "git_show",
    "git_branch",
];

/// Аргумент пути к репозиторию у инструментов `mcp-server-git`. Свой сервер
/// его не объявляет и берёт репозиторий только из `--repository`; клиент
/// всё равно убирает его из схем и подставляет сам — защита не должна
/// зависеть от того, какой сервер запущен.
const REPO_PATH_ARG: &str = "repo_path";

/// Запуск: процесс, рукопожатие и `tools/list`. Локальному бинарнику
/// хватает долей секунды; предел ловит зависший процесс.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Один вызов инструмента.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Штатная остановка сервиса `rmcp`, после неё — `kill`.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Предел результата для модели: вывод `git_diff` большого изменения иначе
/// займёт весь контекст.
pub const MAX_RESULT_CHARS: usize = 16_000;
/// Предел результата в журнале обмена.
const MAX_LOG_RESULT_CHARS: usize = 4_000;

const CALL_URL: &str = "mcp+stdio://agentcli-git-mcp/tools/call";
const LIST_URL: &str = "mcp+stdio://agentcli-git-mcp/tools/list";

/// Где искать сервер: рядом с исполняемым файлом `agentcli` — туда его
/// кладут `cargo build` (`target/<профиль>/`) и `cargo install` (`~/.cargo/bin`),
/// — иначе по `PATH`. Тестовый бинарник лежит в `target/<профиль>/deps/`,
/// поэтому проверяется и родительский каталог `deps`.
pub fn server_program() -> PathBuf {
    let name = format!("{SERVER_NAME}{}", std::env::consts::EXE_SUFFIX);
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        let mut candidates = vec![dir.join(&name)];
        if dir.file_name().is_some_and(|dir_name| dir_name == "deps")
            && let Some(parent) = dir.parent()
        {
            candidates.push(parent.join(&name));
        }
        if let Some(found) = candidates.into_iter().find(|candidate| candidate.is_file()) {
            return found;
        }
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

/// Проверка пути до запуска: каталог существует и содержит `.git` (файл у
/// рабочего дерева `git worktree`, каталог — у обычного репозитория).
/// Возвращается канонический путь: он же ключ реестра процессов.
pub fn validate_repository(path: &str) -> std::result::Result<PathBuf, AgentError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(unavailable(
            "не задан путь к репозиторию: укажите его в настройках чата (Ctrl+P → «Инструменты») \
             или выполните: agentcli config git-tools set --repository <ПУТЬ>",
        ));
    }
    let expanded = match trimmed.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|home| home.join(rest)).unwrap_or_else(|| PathBuf::from(trimmed)),
        None => PathBuf::from(trimmed),
    };
    if !expanded.is_dir() {
        return Err(unavailable(format!("каталог {} не существует", expanded.display())));
    }
    if !expanded.join(".git").exists() {
        return Err(unavailable(format!(
            "{} — не git-репозиторий (нет .git)",
            expanded.display()
        )));
    }
    expanded
        .canonicalize()
        .map_err(|err| unavailable(format!("не удалось разрешить путь {}: {err}", expanded.display())))
}

/// Описание инструмента сервера в том виде, в каком его держит клиент.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Схема аргументов без `repo_path`: свойство и его упоминание в
/// `required` удаляются, путь подставит клиент.
pub fn strip_repo_path(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove(REPO_PATH_ARG);
        }
        if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| name.as_str() != Some(REPO_PATH_ARG));
        }
    }
    schema
}

/// Разрешены ли инструменту обращения модели: читающие — всегда, пишущие —
/// только перечисленные в `git_allowed_tools`.
pub fn is_allowed(name: &str, allowed_writes: Option<&[String]>) -> bool {
    is_read_only(name) || allowed_writes.is_some_and(|list| list.iter().any(|item| item == name))
}

/// Описания для модели: отфильтрованные по разрешениям, без `repo_path`.
/// `description` берётся как есть, аннотации не используются.
pub fn prepare_specs(tools: &[ServerTool], allowed_writes: Option<&[String]>) -> Vec<ToolSpec> {
    tools
        .iter()
        .filter(|tool| is_allowed(&tool.name, allowed_writes))
        .map(|tool| ToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: strip_repo_path(&tool.input_schema),
        })
        .collect()
}

/// Аргументы вызова с подставленным путём репозитория. Значение модели
/// перезаписывается: модель не может обратиться к другому репозиторию, даже
/// если сервер этого не запрещает.
pub fn with_repo_path(arguments: &Value, repo: &Path) -> std::result::Result<serde_json::Map<String, Value>, String> {
    let mut object = match arguments {
        Value::Object(object) => object.clone(),
        Value::Null => serde_json::Map::new(),
        Value::String(raw) => {
            return Err(format!("аргументы вызова — не JSON-объект: {raw}"));
        }
        other => return Err(format!("аргументы вызова — не JSON-объект: {other}")),
    };
    object.insert(
        REPO_PATH_ARG.to_string(),
        Value::String(repo.to_string_lossy().into_owned()),
    );
    Ok(object)
}

/// Обрезка по символам с пометкой о числе отброшенных.
fn truncate(text: String, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n[… обрезано: отброшено {} символов]", total - max_chars)
}

/// Текст результата для модели: текстовые элементы `content` через перевод
/// строки, прочие типы — пометкой, ошибка — с префиксом.
pub fn format_result(content: &[Value], is_error: bool) -> String {
    let parts: Vec<String> = content
        .iter()
        .map(|item| match item.get("type").and_then(Value::as_str) {
            Some("text") => item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Some(kind) => format!("[{kind} опущен]"),
            None => "[элемент без типа опущен]".to_string(),
        })
        .collect();
    let text = parts.join("\n");
    let text = if is_error {
        format!("Ошибка инструмента: {text}")
    } else {
        text
    };
    truncate(text, MAX_RESULT_CHARS)
}

/// Работающий процесс сервера и его инструменты.
struct Running {
    service: RunningService<RoleClient, ()>,
}

/// Процесс `agentcli-git-mcp` одного репозитория.
pub struct GitToolServer {
    repository: PathBuf,
    /// Программа и аргументы до `--repository`: в тестах — несуществующая
    /// команда вместо сервера.
    program: String,
    args: Vec<String>,
    log: Arc<ExchangeLog>,
    /// Список инструментов фиксируется на время жизни процесса:
    /// `notifications/tools/list_changed` не обрабатывается, набор у
    /// сервера статичный.
    tools: Vec<ServerTool>,
    running: Mutex<Option<Running>>,
}

impl GitToolServer {
    /// Запуск `agentcli-git-mcp --repository <путь>` с проверкой пути,
    /// рукопожатием и полным `tools/list`.
    pub async fn start(repository: &str, log: Arc<ExchangeLog>) -> Result<Arc<Self>> {
        let program = server_program();
        Self::start_with(&program.to_string_lossy(), &[], repository, log).await
    }

    pub async fn start_with(
        program: &str,
        args: &[&str],
        repository: &str,
        log: Arc<ExchangeLog>,
    ) -> Result<Arc<Self>> {
        let repository = validate_repository(repository)?;
        let mut server = Self {
            repository,
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            log,
            tools: Vec::new(),
            running: Mutex::new(None),
        };
        let (running, tools) = server.spawn().await?;
        server.tools = tools;
        server.log_start();
        *server.running.get_mut() = Some(running);
        Ok(Arc::new(server))
    }

    pub fn tools(&self) -> &[ServerTool] {
        &self.tools
    }

    async fn spawn(&self) -> std::result::Result<(Running, Vec<ServerTool>), AgentError> {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .arg("--repository")
            .arg(&self.repository)
            .kill_on_drop(true);
        // stderr сервера рисовал бы поверх TUI.
        let (transport, _stderr) = TokioChildProcess::builder(command)
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    unavailable(format!(
                        "не найден {}: соберите его вместе с agentcli (cargo build в agent-cli) \
                         или установите рядом с agentcli (cargo install --path crates/git-mcp)",
                        self.program
                    ))
                } else {
                    unavailable(format!("процесс не запустился: {err}"))
                }
            })?;
        let started = tokio::time::timeout(START_TIMEOUT, async {
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
        .map_err(|_| {
            unavailable(format!(
                "сервер не запустился за {} с",
                START_TIMEOUT.as_secs()
            ))
        })??;
        let (service, tools) = started;
        let tools = tools
            .into_iter()
            .map(|tool| ServerTool {
                name: tool.name.to_string(),
                description: tool.description.map(|d| d.to_string()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect();
        Ok((Running { service }, tools))
    }

    fn log_start(&self) {
        let names: Vec<&str> = self.tools.iter().map(|tool| tool.name.as_str()).collect();
        self.log.log_request(&RequestLogEntry {
            id: &request_id(),
            timestamp: unix_timestamp(),
            url: LIST_URL,
            model: "",
            request: serde_json::json!({
                "repository": self.repository.to_string_lossy(),
                "tools": names,
            }),
        });
    }

    /// Вызов инструмента. Текст для модели возвращается и при ошибке
    /// инструмента, и при протокольной ошибке, и по таймауту: модель умеет
    /// с ними работать. `Err` — только если процесс упал и не перезапустился.
    pub async fn call(&self, name: &str, arguments: &Value) -> Result<String> {
        let arguments = match with_repo_path(arguments, &self.repository) {
            Ok(arguments) => arguments,
            Err(message) => return Ok(format!("Ошибка инструмента: {message}")),
        };
        let id = request_id();
        self.log.log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url: CALL_URL,
            model: name,
            request: serde_json::json!({ "name": name, "arguments": arguments }),
        });
        let started_at = Instant::now();

        let mut guard = self.running.lock().await;
        if guard.is_none() {
            *guard = Some(self.respawn().await?);
        }
        let peer = guard.as_ref().expect("процесс запущен").service.peer().clone();
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        let outcome = tokio::time::timeout(CALL_TIMEOUT, peer.call_tool(params)).await;

        let (status, text, restart) = match outcome {
            Ok(Ok(result)) => {
                let content = serde_json::to_value(&result.content)
                    .ok()
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default();
                let is_error = result.is_error == Some(true);
                (if is_error { 500 } else { 200 }, format_result(&content, is_error), false)
            }
            Ok(Err(ServiceError::McpError(error))) => {
                // Протокольная ошибка (неизвестный инструмент, неверные
                // аргументы) — не ошибка хода: модель может исправиться.
                (500, format!("Ошибка инструмента: {}", error.message), false)
            }
            Ok(Err(error)) => (
                500,
                format!("Ошибка инструмента: сервер инструментов перестал отвечать ({error}); он перезапущен"),
                true,
            ),
            // Состояние зависшего сервера неизвестно: перед следующим
            // вызовом он перезапускается.
            Err(_) => (
                504,
                format!("Ошибка инструмента: вызов не уложился в {} с", CALL_TIMEOUT.as_secs()),
                true,
            ),
        };

        self.log.log_response(&ResponseLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            status,
            duration_ms: started_at.elapsed().as_millis(),
            response: Value::String(truncate(text.clone(), MAX_LOG_RESULT_CHARS)),
        });

        if restart {
            if let Some(running) = guard.take() {
                stop(running).await;
            }
            // Одна попытка перезапуска: неудача прерывает ход.
            *guard = Some(self.respawn().await?);
        }
        Ok(text)
    }

    async fn respawn(&self) -> std::result::Result<Running, AgentError> {
        let (running, _tools) = self.spawn().await?;
        Ok(running)
    }

    /// Остановка: штатная отмена сервиса `rmcp` с ожиданием, затем `kill`
    /// (его делает сам транспорт при уничтожении процесса).
    pub async fn shutdown(&self) {
        if let Some(running) = self.running.lock().await.take() {
            stop(running).await;
        }
    }
}

async fn stop(running: Running) {
    let mut service = running.service;
    let _ = tokio::time::timeout(STOP_TIMEOUT, service.close()).await;
}

/// Git-инструменты одного хода: сервер плюс разрешения чата.
pub struct GitTools {
    pub server: Arc<GitToolServer>,
    pub allowed_writes: Option<Vec<String>>,
}

#[async_trait]
impl ToolExecutor for GitTools {
    fn specs(&self) -> Vec<ToolSpec> {
        prepare_specs(self.server.tools(), self.allowed_writes.as_deref())
    }

    fn is_write(&self, name: &str) -> bool {
        !is_read_only(name)
    }

    async fn call(&self, call: &ToolCall) -> Result<String> {
        self.server.call(&call.name, &call.arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ServerTool {
        ServerTool {
            name: name.into(),
            description: Some(format!("{name} description")),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string" },
                    "max_count": { "type": "integer" }
                },
                "required": ["repo_path"]
            }),
        }
    }

    #[test]
    fn unknown_tool_is_treated_as_write() {
        assert!(is_read_only("git_status"));
        assert!(is_read_only("git_log"));
        assert!(!is_read_only("git_commit"));
        assert!(!is_read_only("git_add"));
        assert!(!is_read_only("git_push_future_version"));
    }

    #[test]
    fn repo_path_is_removed_from_schema() {
        let schema = strip_repo_path(&tool("git_log").input_schema);
        assert!(schema["properties"].get("repo_path").is_none());
        assert_eq!(schema["properties"]["max_count"]["type"], "integer");
        assert_eq!(schema["required"], json!([]));
    }

    #[test]
    fn specs_keep_reads_and_only_listed_writes() {
        let tools = [tool("git_status"), tool("git_add"), tool("git_commit")];
        let names = |specs: Vec<ToolSpec>| specs.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(prepare_specs(&tools, None)), vec!["git_status"]);
        let allowed = vec!["git_add".to_string()];
        assert_eq!(names(prepare_specs(&tools, Some(&allowed))), vec!["git_status", "git_add"]);
    }

    #[test]
    fn repo_path_is_substituted_over_model_value() {
        let repo = Path::new("/tmp/настоящий");
        let args = with_repo_path(&json!({ "repo_path": "/etc", "max_count": 3 }), repo).unwrap();
        assert_eq!(args["repo_path"], "/tmp/настоящий");
        assert_eq!(args["max_count"], 3);
        assert!(with_repo_path(&Value::String("{сломано".into()), repo).is_err());
        assert_eq!(with_repo_path(&Value::Null, repo).unwrap().len(), 1);
    }

    #[test]
    fn result_content_is_joined_and_marked() {
        let content = [
            json!({ "type": "text", "text": "строка 1" }),
            json!({ "type": "image", "data": "…", "mimeType": "image/png" }),
            json!({ "type": "text", "text": "строка 2" }),
        ];
        assert_eq!(format_result(&content, false), "строка 1\n[image опущен]\nстрока 2");
        assert_eq!(
            format_result(&content[..1], true),
            "Ошибка инструмента: строка 1"
        );
    }

    #[test]
    fn long_result_is_truncated_with_note() {
        let long = "я".repeat(MAX_RESULT_CHARS + 10);
        let text = format_result(&[json!({ "type": "text", "text": long })], false);
        assert!(text.ends_with("[… обрезано: отброшено 10 символов]"));
        assert!(text.chars().count() < MAX_RESULT_CHARS + 60);
    }

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentcli-mcp-{name}-{}", request_id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[test]
    fn repository_must_exist_and_contain_git() {
        let missing = validate_repository("/definitely/not/here").unwrap_err();
        assert!(matches!(missing, AgentError::ToolServerUnavailable { .. }));
        let plain = std::env::temp_dir();
        assert!(validate_repository(plain.to_str().unwrap()).is_err());
        let repo = temp_repo("valid");
        assert!(validate_repository(repo.to_str().unwrap()).is_ok());
        assert!(validate_repository("  ").is_err());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn missing_server_binary_is_server_unavailable() {
        let repo = temp_repo("no-server");
        let err = GitToolServer::start_with(
            "agentcli-no-such-git-mcp-binary",
            &[],
            repo.to_str().unwrap(),
            Arc::new(ExchangeLog::disabled()),
        )
        .await
        .err()
        .expect("ошибка запуска");
        match err.downcast_ref::<AgentError>() {
            Some(AgentError::ToolServerUnavailable { server, reason }) => {
                assert_eq!(server, SERVER_NAME);
                assert!(reason.contains("cargo install --path crates/git-mcp"), "причина: {reason}");
            }
            other => panic!("ожидался ToolServerUnavailable, получено: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(repo);
    }

    /// Живой тест с настоящим `agentcli-git-mcp`: бинарник должен быть
    /// собран (`cargo build -p agentcli-git-mcp`), поэтому тест под `ignore` —
    /// `cargo test -p agentcli` сервер не собирает.
    #[tokio::test]
    #[ignore]
    async fn live_server_reads_status_of_temp_repository() {
        let dir = std::env::temp_dir().join(format!("agentcli-mcp-live-{}", request_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .expect("git init");
        assert!(status.success());
        std::fs::write(dir.join("README.md"), "demo\n").unwrap();

        let server = GitToolServer::start(dir.to_str().unwrap(), Arc::new(ExchangeLog::disabled()))
            .await
            .expect("запуск agentcli-git-mcp");
        let names: Vec<&str> = server.tools().iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"git_status"), "инструменты: {names:?}");
        let text = server.call("git_status", &json!({})).await.expect("git_status");
        assert!(text.contains("README.md"), "вывод: {text}");
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
