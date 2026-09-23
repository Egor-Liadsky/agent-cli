//! `agentcli-git-mcp` — MCP-сервер git-инструментов для `agentcli`.
//!
//! Отдельный бинарник, а не подкоманда `agentcli`: у сервера свой граф
//! зависимостей (серверная часть `rmcp`, `schemars`) без терминальных
//! крейтов клиента, и его можно подключить к любому MCP-клиенту.
//!
//! Имена и аргументы инструментов совпадают с `mcp-server-git`, чтобы
//! клиент (классификация читающих и пишущих, подтверждение) не зависел от
//! того, какой из серверов запущен. Аргумент `repo_path`, который есть у
//! инструментов `mcp-server-git`, здесь не объявлен и игнорируется:
//! репозиторий задаётся только ключом `--repository` при запуске.
//!
//! Запуск: `agentcli-git-mcp --repository <путь>`; протокол — JSON-RPC
//! через stdin/stdout.

mod git;

use git::{positional, GitResult, Repo};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;

/// Строк контекста вокруг изменения по умолчанию — как у `git diff`.
const DEFAULT_CONTEXT_LINES: u32 = 3;
/// Коммитов в `git_log` по умолчанию.
const DEFAULT_LOG_COUNT: u32 = 10;

fn default_context_lines() -> u32 {
    DEFAULT_CONTEXT_LINES
}

fn default_log_count() -> u32 {
    DEFAULT_LOG_COUNT
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffArgs {
    /// Number of context lines around each change.
    #[serde(default = "default_context_lines")]
    context_lines: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffTargetArgs {
    /// Branch, tag or commit to compare the working tree with.
    target: String,
    /// Number of context lines around each change.
    #[serde(default = "default_context_lines")]
    context_lines: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CommitArgs {
    /// Commit message.
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddArgs {
    /// Paths to stage, relative to the repository root.
    files: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogArgs {
    /// Maximum number of commits to show.
    #[serde(default = "default_log_count")]
    max_count: u32,
    /// Show commits after this date (ISO 8601 or a git date such as "2 weeks ago").
    #[serde(default)]
    start_timestamp: Option<String>,
    /// Show commits before this date.
    #[serde(default)]
    end_timestamp: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateBranchArgs {
    /// Name of the new branch.
    branch_name: String,
    /// Branch or commit to start from; the current HEAD if omitted.
    #[serde(default)]
    base_branch: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CheckoutArgs {
    /// Branch to switch to.
    branch_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ShowArgs {
    /// Commit, tag or other revision to show.
    revision: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BranchArgs {
    /// Which branches to list: "local", "remote" or "all".
    branch_type: String,
    /// Only branches that contain this commit.
    #[serde(default)]
    contains: Option<String>,
    /// Only branches that do not contain this commit.
    #[serde(default)]
    not_contains: Option<String>,
}

#[derive(Debug, Clone)]
struct GitServer {
    repo: Repo,
    tool_router: ToolRouter<Self>,
}

/// Вывод команды с заголовком; пустой вывод заменяется пояснением, чтобы
/// модель не приняла пустую строку за сбой.
fn titled(title: &str, output: String, empty: &str) -> String {
    if output.trim().is_empty() {
        format!("{title}\n{empty}")
    } else {
        format!("{title}\n{output}")
    }
}

#[tool_router]
impl GitServer {
    fn new(repo: Repo) -> Self {
        Self {
            repo,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Shows the working tree status")]
    async fn git_status(&self) -> GitResult {
        let output = self.repo.run(&["status"]).await?;
        Ok(format!("Repository status:\n{output}"))
    }

    #[tool(description = "Shows changes in the working directory that are not yet staged")]
    async fn git_diff_unstaged(&self, Parameters(args): Parameters<DiffArgs>) -> GitResult {
        let unified = format!("--unified={}", args.context_lines);
        let output = self.repo.run(&["diff", &unified]).await?;
        Ok(titled("Unstaged changes:", output, "(no unstaged changes)"))
    }

    #[tool(description = "Shows changes that are staged for commit")]
    async fn git_diff_staged(&self, Parameters(args): Parameters<DiffArgs>) -> GitResult {
        let unified = format!("--unified={}", args.context_lines);
        let output = self.repo.run(&["diff", "--cached", &unified]).await?;
        Ok(titled("Staged changes:", output, "(no staged changes)"))
    }

    #[tool(description = "Shows differences between the working tree and a branch or commit")]
    async fn git_diff(&self, Parameters(args): Parameters<DiffTargetArgs>) -> GitResult {
        let target = positional("target", &args.target)?;
        let unified = format!("--unified={}", args.context_lines);
        let output = self.repo.run(&["diff", &unified, target, "--"]).await?;
        Ok(titled(&format!("Diff with {target}:"), output, "(no differences)"))
    }

    #[tool(description = "Records staged changes to the repository")]
    async fn git_commit(&self, Parameters(args): Parameters<CommitArgs>) -> GitResult {
        if args.message.trim().is_empty() {
            return Err("сообщение коммита не может быть пустым".to_string());
        }
        // Сообщение идёт значением флага `-m`, а не позиционным аргументом,
        // поэтому ведущий «-» в нём безопасен.
        self.repo.run(&["commit", "-m", &args.message]).await?;
        let hash = self.repo.run(&["rev-parse", "HEAD"]).await?;
        Ok(format!("Changes committed successfully with hash {}", hash.trim()))
    }

    #[tool(description = "Adds file contents to the staging area")]
    async fn git_add(&self, Parameters(args): Parameters<AddArgs>) -> GitResult {
        if args.files.is_empty() {
            return Err("список файлов пуст".to_string());
        }
        // Пути идут после `--`: git не прочтёт их как флаги и сам отклонит
        // путь за пределами репозитория.
        let mut command = vec!["add", "--"];
        command.extend(args.files.iter().map(String::as_str));
        self.repo.run(&command).await?;
        Ok("Files staged successfully".to_string())
    }

    #[tool(description = "Unstages all staged changes")]
    async fn git_reset(&self) -> GitResult {
        self.repo.run(&["reset", "--quiet"]).await?;
        Ok("All staged changes reset".to_string())
    }

    #[tool(description = "Shows the commit logs")]
    async fn git_log(&self, Parameters(args): Parameters<LogArgs>) -> GitResult {
        let count = format!("--max-count={}", args.max_count.max(1));
        let format = "--format=Commit: %H%nAuthor: %an <%ae>%nDate: %aI%nMessage: %s%n".to_string();
        let mut command = vec!["log".to_string(), count, format];
        if let Some(since) = args.start_timestamp.as_deref() {
            command.push(format!("--since={}", positional("start_timestamp", since)?));
        }
        if let Some(until) = args.end_timestamp.as_deref() {
            command.push(format!("--until={}", positional("end_timestamp", until)?));
        }
        let command: Vec<&str> = command.iter().map(String::as_str).collect();
        let output = self.repo.run(&command).await?;
        Ok(titled("Commit history:", output, "(no commits)"))
    }

    #[tool(description = "Creates a new branch from an optional base branch")]
    async fn git_create_branch(&self, Parameters(args): Parameters<CreateBranchArgs>) -> GitResult {
        let name = positional("branch_name", &args.branch_name)?;
        let mut command = vec!["branch", name];
        let base = match args.base_branch.as_deref() {
            Some(base) => {
                let base = positional("base_branch", base)?;
                command.push(base);
                base.to_string()
            }
            None => "HEAD".to_string(),
        };
        self.repo.run(&command).await?;
        Ok(format!("Created branch '{name}' from '{base}'"))
    }

    #[tool(description = "Switches branches")]
    async fn git_checkout(&self, Parameters(args): Parameters<CheckoutArgs>) -> GitResult {
        let name = positional("branch_name", &args.branch_name)?;
        // `--` в конце: имя трактуется как ветка, а не как путь к файлу.
        self.repo.run(&["checkout", name, "--"]).await?;
        Ok(format!("Switched to branch '{name}'"))
    }

    #[tool(description = "Shows the contents of a commit")]
    async fn git_show(&self, Parameters(args): Parameters<ShowArgs>) -> GitResult {
        let revision = positional("revision", &args.revision)?;
        self.repo.run(&["show", revision, "--"]).await
    }

    #[tool(description = "Lists git branches")]
    async fn git_branch(&self, Parameters(args): Parameters<BranchArgs>) -> GitResult {
        let mut command = vec!["branch".to_string()];
        match args.branch_type.as_str() {
            "local" => {}
            "remote" => command.push("--remotes".to_string()),
            "all" => command.push("--all".to_string()),
            other => {
                return Err(format!(
                    "branch_type должен быть local, remote или all, получено: {other}"
                ));
            }
        }
        if let Some(commit) = args.contains.as_deref() {
            command.push(format!("--contains={}", positional("contains", commit)?));
        }
        if let Some(commit) = args.not_contains.as_deref() {
            command.push(format!("--no-contains={}", positional("not_contains", commit)?));
        }
        let command: Vec<&str> = command.iter().map(String::as_str).collect();
        let output = self.repo.run(&command).await?;
        Ok(titled("Branches:", output, "(no branches)"))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Git tools for the repository {}. Paths are relative to its root.",
                self.repo.root().display()
            ))
    }
}

/// Путь из `--repository <путь>` (или `--repository=<путь>`).
fn repository_arg(args: impl Iterator<Item = String>) -> Result<PathBuf, String> {
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "--repository" {
            return args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "после --repository нужен путь".to_string());
        }
        if let Some(path) = arg.strip_prefix("--repository=") {
            return Ok(PathBuf::from(path));
        }
    }
    Err("использование: agentcli-git-mcp --repository <путь к git-репозиторию>".to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Проверка до рукопожатия: с неверным путём клиент получает понятный
    // отказ процесса, а не сервер, у которого падает каждый вызов.
    let repo = repository_arg(std::env::args().skip(1))
        .and_then(|path| Repo::open(&path))
        .map_err(anyhow::Error::msg)?;
    let service = GitServer::new(repo).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_is_read_from_arguments() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
        assert_eq!(repository_arg(args(&["--repository", "/r"])), Ok(PathBuf::from("/r")));
        assert_eq!(repository_arg(args(&["--repository=/r"])), Ok(PathBuf::from("/r")));
        assert!(repository_arg(args(&["--repository"])).is_err());
        assert!(repository_arg(args(&[])).is_err());
    }
}
