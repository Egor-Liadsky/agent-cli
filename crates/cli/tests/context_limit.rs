//! Интеграционный тест команды `agentcli config context-limit`: запускает
//! собранный бинарник в изолированном `$HOME`, чтобы не трогать реальный
//! конфиг пользователя (`Config::path()` не принимает путь параметром).

use std::path::PathBuf;
use std::process::{Command, Output};

fn run(home: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentcli"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("запуск agentcli")
}

fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentcli-context-limit-test-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("создать временный HOME");
    dir
}

#[test]
fn set_show_clear_and_reject_zero() {
    let home = temp_home("basic");

    let show_empty = run(&home, &["config", "context-limit", "show"]);
    assert!(show_empty.status.success());
    assert!(
        String::from_utf8_lossy(&show_empty.stdout).contains("не задан"),
        "лимит изначально не задан: {}",
        String::from_utf8_lossy(&show_empty.stdout)
    );

    let set = run(&home, &["config", "context-limit", "set", "4000"]);
    assert!(
        set.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&set.stderr)
    );

    let show_set = run(&home, &["config", "context-limit", "show"]);
    assert!(show_set.status.success());
    assert!(
        String::from_utf8_lossy(&show_set.stdout).contains("4000"),
        "лимит сохранён: {}",
        String::from_utf8_lossy(&show_set.stdout)
    );

    let set_zero = run(&home, &["config", "context-limit", "set", "0"]);
    assert!(!set_zero.status.success(), "0 должен быть отклонён");

    let show_after_zero = run(&home, &["config", "context-limit", "show"]);
    assert!(
        String::from_utf8_lossy(&show_after_zero.stdout).contains("4000"),
        "отклонённый ввод не должен менять сохранённое значение: {}",
        String::from_utf8_lossy(&show_after_zero.stdout)
    );

    let clear = run(&home, &["config", "context-limit", "clear"]);
    assert!(clear.status.success());

    let show_cleared = run(&home, &["config", "context-limit", "show"]);
    assert!(
        String::from_utf8_lossy(&show_cleared.stdout).contains("не задан"),
        "лимит снят: {}",
        String::from_utf8_lossy(&show_cleared.stdout)
    );

    let _ = std::fs::remove_dir_all(&home);
}
