## 1. Обёртка над rfd

- [x] 1.1 Добавить `rfd` (без фич по умолчанию, `xdg-portal`) в
  `crates/cli/Cargo.toml`; проверка — `grep -n rfd crates/core/Cargo.toml`
  пуст, `cargo build` проходит.
- [x] 1.2 Создать `crates/cli/src/folder_picker.rs`: `pick_folder`,
  `gui_unavailable_reason`, `start_dir`; покрыть тестами без вызова
  диалога; проверка — `cargo test -p agentcli folder_picker`.

## 2. Интеграция в TUI

- [x] 2.1 Запрос выбора `AppState::folder_pick` и клавиша `Ctrl+X` в
  `handle_settings_key` только на полях-каталогах.
- [x] 2.2 Приостановка и восстановление терминала вокруг диалога в
  `run_app`, применение результата к полю и сообщение в статусе; тесты
  на применение результата и отмену.
- [x] 2.3 Подсказка в нижней строке настроек и описаниях полей.

## 3. Документация и проверка

- [x] 3.1 README: клавиша, стартовый каталог, поведение без графической
  сессии.
- [x] 3.2 `cargo build --release`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`.
