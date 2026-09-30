//! Интеграционный тест `agentcli config index` и ошибок `agentcli index`:
//! собранный бинарник в изолированном `$HOME`, реальный конфиг не
//! затрагивается. Сервер `index-mcp` здесь не нужен — сквозной прогон с ним
//! лежит в `live_index` (`#[ignore]`).

use std::path::PathBuf;
use std::process::{Command, Output};

fn run(home: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentcli"))
        .args(args)
        .env("HOME", home)
        // Путь, которого нет: команда должна назвать его, а не искать сервер в PATH.
        .env("AGENTCLI_INDEX_MCP", home.join("нет-такого-index-mcp"))
        .output()
        .expect("запуск agentcli")
}

fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agentcli-index-config-test-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("создать временный HOME");
    dir
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn set_show_clear_and_validation() {
    let home = temp_home("basic");

    let empty = run(&home, &["config", "index", "show"]);
    assert!(empty.status.success());
    assert!(stdout(&empty).contains("выключены"), "{}", stdout(&empty));

    let set = run(
        &home,
        &[
            "config", "index", "set", "--root", "/notes", "--db", "/tmp/idx.db", "--strategy", "structure", "--model", "bge-m3",
            "--unit", "tokens", "--chunk-size", "400", "--max-section", "600",
        ],
    );
    assert!(set.status.success(), "stderr: {}", stderr(&set));
    let shown = stdout(&run(&home, &["config", "index", "show"]));
    for expected in ["включены", "/notes", "/tmp/idx.db", "structure", "bge-m3", "tokens", "400", "600"] {
        assert!(shown.contains(expected), "нет «{expected}» в: {shown}");
    }
    // Не названное осталось умолчанием.
    assert!(shown.contains("200 (умолчание)"), "{shown}");

    // Неверная стратегия и единица отклоняются и ничего не меняют.
    let bad = run(&home, &["config", "index", "set", "--strategy", "semantic"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("Доступны: fixed, structure, all"), "{}", stderr(&bad));
    assert!(!run(&home, &["config", "index", "set", "--unit", "words"]).status.success());
    assert!(!run(&home, &["config", "index", "set"]).status.success(), "нужно хоть одно значение");
    assert!(stdout(&run(&home, &["config", "index", "show"])).contains("structure"));

    // Пустая строка (у чисел — 0) снимает отдельное значение.
    assert!(run(&home, &["config", "index", "set", "--model", "", "--chunk-size", "0"]).status.success());
    let shown = stdout(&run(&home, &["config", "index", "show"]));
    assert!(shown.contains("nomic-embed-text (умолчание)"), "{shown}");
    assert!(shown.contains("1200 (умолчание)"), "{shown}");

    assert!(run(&home, &["config", "index", "clear"]).status.success());
    assert!(stdout(&run(&home, &["config", "index", "show"])).contains("выключены"));
}

#[test]
fn commands_explain_what_is_missing() {
    let home = temp_home("missing");

    // Без базы в конфиге искать не в чем.
    let search = run(&home, &["index", "search", "вопрос"]);
    assert!(!search.status.success());
    assert!(stderr(&search).contains("config index set --db"), "{}", stderr(&search));

    // База есть, каталога нет: сборка называет флаг.
    assert!(run(&home, &["config", "index", "set", "--db", "/tmp/idx-missing.db"]).status.success());
    let build = run(&home, &["index", "build"]);
    assert!(!build.status.success());
    assert!(stderr(&build).contains("--root"), "{}", stderr(&build));

    // Сервер не найден: ошибка называет путь и переменную.
    let status = run(&home, &["index", "status"]);
    assert!(!status.status.success());
    let err = stderr(&status);
    assert!(err.contains("нет-такого-index-mcp") && err.contains("AGENTCLI_INDEX_MCP"), "{err}");
}
