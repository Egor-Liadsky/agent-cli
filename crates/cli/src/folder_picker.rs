//! Выбор каталога нативным диалогом ОС для полей-путей в настройках TUI.
//!
//! `rfd` выбран потому, что даёт один API поверх NSOpenPanel (macOS),
//! диалога Windows и портала XDG (Linux), а не набор вызовов `osascript`,
//! `zenity` и т. п. под каждую ОС. Фичи по умолчанию отключены: портал XDG
//! ходит через D-Bus и не требует при сборке библиотек GTK или Wayland.
//!
//! Модуль знает только про диалог; терминал на время вызова освобождает
//! `tui.rs`, у которого он есть.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

/// Открыть системный диалог выбора папки. `Ok(None)` — пользователь закрыл
/// диалог без выбора.
///
/// Вызов синхронный и должен идти из главного потока: AppKit открывает
/// окна только оттуда, поэтому обёртки вроде `spawn_blocking` здесь
/// неуместны — они унесли бы вызов на рабочий поток.
pub fn pick_folder(start: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(reason) = gui_unavailable() {
        bail!("{reason}");
    }
    // Сбой бэкенда внутри `rfd` бывает паникой, а не ошибкой; TUI в этот
    // момент без raw-режима, и паника оставила бы терминал в полусломанном
    // состоянии, поэтому она превращается в обычную ошибку.
    panic::catch_unwind(AssertUnwindSafe(|| {
        let mut dialog = rfd::FileDialog::new().set_title("Выбор каталога");
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        dialog.pick_folder()
    }))
    .map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "неизвестная причина".to_string());
        anyhow!("сбой диалога: {message}")
    })
}

/// Причина, по которой графический диалог открыть нельзя, для текущего
/// процесса.
pub fn gui_unavailable() -> Option<&'static str> {
    gui_unavailable_reason(std::env::consts::OS, |name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Причина, по которой диалог открыть нельзя, по ОС и наличию переменных
/// окружения. `rfd` не отличает отмену от сбоя (оба — `None`), а над SSH
/// панель macOS появилась бы на экране удалённой машины, поэтому
/// недоступность определяется заранее. Окружение передаётся явно, чтобы
/// проверка тестировалась без графической сессии.
pub fn gui_unavailable_reason(os: &str, has_var: impl Fn(&str) -> bool) -> Option<&'static str> {
    if has_var("SSH_CONNECTION") || has_var("SSH_TTY") {
        return Some("сеанс SSH: окно диалога открылось бы не на вашем экране");
    }
    let unix_desktop = !matches!(os, "macos" | "windows" | "ios" | "android");
    if unix_desktop && !has_var("DISPLAY") && !has_var("WAYLAND_DISPLAY") {
        return Some("нет графической сессии (не заданы DISPLAY и WAYLAND_DISPLAY)");
    }
    None
}

/// Стартовый каталог диалога: значение поля (с раскрытием ведущего `~`),
/// если такой каталог есть, иначе домашний.
pub fn start_dir(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    let value = value.trim();
    let expanded = if value == "~" {
        home.map(Path::to_path_buf)
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.map(|home| home.join(rest))
    } else if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    };
    expanded
        .filter(|path| path.is_dir())
        .or_else(|| home.map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |name| vars.contains(&name)
    }

    #[test]
    fn ssh_session_has_no_gui_on_any_os() {
        for os in ["macos", "linux", "windows"] {
            assert!(gui_unavailable_reason(os, env(&["SSH_CONNECTION", "DISPLAY"])).is_some());
            assert!(gui_unavailable_reason(os, env(&["SSH_TTY"])).is_some());
        }
    }

    #[test]
    fn local_macos_and_windows_have_gui_without_display() {
        assert_eq!(gui_unavailable_reason("macos", env(&[])), None);
        assert_eq!(gui_unavailable_reason("windows", env(&[])), None);
    }

    #[test]
    fn linux_needs_display_or_wayland() {
        assert!(gui_unavailable_reason("linux", env(&[])).is_some());
        assert_eq!(gui_unavailable_reason("linux", env(&["DISPLAY"])), None);
        assert_eq!(gui_unavailable_reason("freebsd", env(&["WAYLAND_DISPLAY"])), None);
    }

    #[test]
    fn start_dir_uses_existing_field_value() {
        let dir = std::env::temp_dir();
        let home = Path::new("/nonexistent-home");
        assert_eq!(start_dir(dir.to_str().unwrap(), Some(home)), Some(dir));
    }

    #[test]
    fn start_dir_expands_tilde() {
        let home = std::env::temp_dir();
        let nested = home.join("folder-picker-start-dir");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(start_dir("~/folder-picker-start-dir", Some(&home)), Some(nested.clone()));
        assert_eq!(start_dir("~", Some(&home)), Some(home.clone()));
        std::fs::remove_dir(&nested).unwrap();
    }

    #[test]
    fn start_dir_falls_back_to_home() {
        let home = Path::new("/home-fallback");
        assert_eq!(start_dir("", Some(home)), Some(home.to_path_buf()));
        assert_eq!(start_dir("/definitely/missing/dir", Some(home)), Some(home.to_path_buf()));
        assert_eq!(start_dir("/definitely/missing/dir", None), None);
    }
}
