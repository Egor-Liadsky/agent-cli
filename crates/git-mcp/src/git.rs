//! Запуск системного `git` в одном репозитории.
//!
//! Команды выполняет настоящий `git`, а не библиотека (`git2`, `gix`): вывод
//! ровно такой, какой человек видит в терминале, модель читает его без
//! пересказа, и в графе зависимостей нет C-библиотеки `libgit2`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Ошибка вызова: текст уходит модели результатом-ошибкой.
pub type GitResult = Result<String, String>;

/// Репозиторий, с которым работает сервер. Путь задаётся один раз при
/// запуске и в аргументах инструментов не принимается: модель не может
/// обратиться к другому репозиторию.
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Каталог должен существовать и содержать `.git` (каталог у обычного
    /// репозитория, файл у рабочего дерева `git worktree`).
    pub fn open(path: &Path) -> Result<Self, String> {
        if !path.is_dir() {
            return Err(format!("каталог {} не существует", path.display()));
        }
        if !path.join(".git").exists() {
            return Err(format!("{} — не git-репозиторий (нет .git)", path.display()));
        }
        let root = path
            .canonicalize()
            .map_err(|err| format!("не удалось разрешить путь {}: {err}", path.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `git -C <репозиторий> <args>`. Ненулевой код выхода — ошибка с
    /// текстом stderr.
    pub async fn run(&self, args: &[&str]) -> GitResult {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.root)
            // Пути с кириллицей выводятся как есть, а не восьмеричными
            // escape-последовательностями.
            .args(["-c", "core.quotepath=off"])
            .args(args)
            // Ни пейджера, ни редактора, ни вопроса о пароле: stdin у
            // сервера занят протоколом, и любой интерактив повесил бы вызов.
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|err| format!("не удалось запустить git: {err}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
            Err(format!("git {} завершился с ошибкой: {detail}", args.first().unwrap_or(&"")))
        }
    }
}

/// Значение, которое попадает в командную строку git позиционным
/// аргументом (ревизия, ветка, дата). Начинающееся с `-` git прочитал бы как
/// флаг — например `--output=/путь` у `git diff` записал бы файл вне
/// репозитория, — поэтому такое значение отклоняется.
pub fn positional<'a>(name: &str, value: &'a str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{name} не задан"));
    }
    if value.starts_with('-') {
        return Err(format!("{name} не может начинаться с «-»: {value}"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_like_values_are_rejected() {
        assert_eq!(positional("ревизия", "HEAD~1"), Ok("HEAD~1"));
        assert!(positional("ревизия", "--output=/tmp/x").is_err());
        assert!(positional("ветка", "-D").is_err());
        assert!(positional("ветка", "  ").is_err());
    }

    #[test]
    fn directory_without_git_is_not_a_repository() {
        let dir = std::env::temp_dir();
        assert!(Repo::open(&dir).is_err());
        assert!(Repo::open(Path::new("/definitely/not/here")).is_err());
    }
}
