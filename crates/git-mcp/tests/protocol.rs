//! Сервер проверяется так, как его видит `agentcli`: настоящий процесс,
//! MCP-клиент `rmcp` через stdin/stdout, временный git-репозиторий.

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;

const BINARY: &str = env!("CARGO_BIN_EXE_agentcli-git-mcp");

/// Временный репозиторий с одним коммитом и незакоммиченными правками.
fn temp_repo(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("agentcli-git-mcp-{name}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .stdout(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();
    git(&["add", "notes.txt"]);
    git(&["commit", "-q", "-m", "Initial commit"]);
    std::fs::write(dir.join("notes.txt"), "hello\nnew line\n").unwrap();
    std::fs::write(dir.join("заметки.txt"), "черновик\n").unwrap();
    dir
}

async fn connect(repo: &Path) -> RunningService<RoleClient, ()> {
    let mut command = tokio::process::Command::new(BINARY);
    command.arg("--repository").arg(repo);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .expect("запуск сервера");
    ().serve(transport).await.expect("рукопожатие MCP")
}

/// Текст результата и признак ошибки.
async fn call(client: &RunningService<RoleClient, ()>, name: &str, arguments: Value) -> (String, bool) {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(arguments.as_object().cloned().unwrap_or_default());
    let result = client.peer().call_tool(params).await.expect("tools/call");
    let content = serde_json::to_value(&result.content).unwrap();
    let text = content
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (text, result.is_error == Some(true))
}

#[tokio::test]
async fn lists_the_same_tools_as_mcp_server_git() {
    let repo = temp_repo("list");
    let client = connect(&repo).await;
    let tools = client.peer().list_all_tools().await.expect("tools/list");
    let mut names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "git_add",
            "git_branch",
            "git_checkout",
            "git_commit",
            "git_create_branch",
            "git_diff",
            "git_diff_staged",
            "git_diff_unstaged",
            "git_log",
            "git_reset",
            "git_show",
            "git_status",
        ]
    );
    let log = tools.iter().find(|tool| tool.name == "git_log").unwrap();
    let schema = Value::Object((*log.input_schema).clone());
    assert!(schema["properties"].get("max_count").is_some(), "схема: {schema}");
    assert!(schema["properties"].get("repo_path").is_none());
    let _ = std::fs::remove_dir_all(repo);
}

#[tokio::test]
async fn reads_and_writes_the_repository() {
    let repo = temp_repo("rw");
    let client = connect(&repo).await;
    // Клиент agentcli подставляет repo_path сам — сервер его игнорирует.
    let foreign = json!({ "repo_path": "/etc" });

    let (status, error) = call(&client, "git_status", foreign.clone()).await;
    assert!(!error);
    assert!(status.contains("notes.txt"), "{status}");
    assert!(status.contains("заметки.txt"), "кириллица без escape: {status}");

    let (diff, _) = call(&client, "git_diff_unstaged", json!({})).await;
    assert!(diff.contains("+new line"), "{diff}");

    let (added, error) = call(&client, "git_add", json!({ "files": ["notes.txt"], "repo_path": "/etc" })).await;
    assert!(!error, "{added}");
    let (staged, _) = call(&client, "git_diff_staged", json!({ "context_lines": 0 })).await;
    assert!(staged.contains("+new line"), "{staged}");

    let (committed, error) = call(&client, "git_commit", json!({ "message": "Add new line" })).await;
    assert!(!error, "{committed}");
    assert!(committed.starts_with("Changes committed successfully with hash "));

    let (log, _) = call(&client, "git_log", json!({ "max_count": 1 })).await;
    assert!(log.contains("Message: Add new line"), "{log}");
    assert!(!log.contains("Initial commit"), "max_count соблюдён: {log}");

    let (created, error) = call(&client, "git_create_branch", json!({ "branch_name": "feature" })).await;
    assert!(!error, "{created}");
    let (switched, error) = call(&client, "git_checkout", json!({ "branch_name": "feature" })).await;
    assert!(!error, "{switched}");
    let (branches, _) = call(&client, "git_branch", json!({ "branch_type": "local" })).await;
    assert!(branches.contains("* feature"), "{branches}");

    let (shown, _) = call(&client, "git_show", json!({ "revision": "HEAD" })).await;
    assert!(shown.contains("Add new line"), "{shown}");
    let (versus, _) = call(&client, "git_diff", json!({ "target": "HEAD~1" })).await;
    assert!(versus.contains("+new line"), "{versus}");

    let (reset, error) = call(&client, "git_reset", json!({})).await;
    assert!(!error, "{reset}");
    let _ = std::fs::remove_dir_all(repo);
}

#[tokio::test]
async fn option_like_arguments_and_git_failures_are_tool_errors() {
    let repo = temp_repo("errors");
    let client = connect(&repo).await;

    let (text, error) = call(&client, "git_diff", json!({ "target": "--output=/tmp/owned" })).await;
    assert!(error, "{text}");
    assert!(!Path::new("/tmp/owned").exists());

    let (text, error) = call(&client, "git_checkout", json!({ "branch_name": "no-such-branch" })).await;
    assert!(error, "{text}");
    assert!(text.contains("git checkout"), "{text}");

    let (text, error) = call(&client, "git_add", json!({ "files": ["../outside.txt"] })).await;
    assert!(error, "путь вне репозитория отклоняет git: {text}");

    let (text, error) = call(&client, "git_branch", json!({ "branch_type": "everything" })).await;
    assert!(error, "{text}");
    let _ = std::fs::remove_dir_all(repo);
}

#[test]
fn refuses_to_start_without_a_repository() {
    let output = std::process::Command::new(BINARY)
        .args(["--repository", "/definitely/not/here"])
        .stdin(Stdio::null())
        .output()
        .expect("запуск");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("не существует"));
}
