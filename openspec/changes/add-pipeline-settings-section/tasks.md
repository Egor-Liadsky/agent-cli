## 1. Раздел в TUI

- [x] 1.1 Добавить `FormatField::PipelineRoot`, `FormatField::PipelineOutput`
  и `SettingsSection::Pipeline` после `Activity` (`ALL` — 10 разделов),
  подписи, пояснения и подсказки пустых значений; проверка — `cargo build`.
- [x] 1.2 Поля `pipeline_root`, `pipeline_output` в `SettingsEditor` из
  `Config`, сопоставления в `field_value_mut`, `directory_value_mut`,
  `is_directory`; проверка — тест «раздел отражает Config (заданный и
  пустой)».
- [x] 1.3 `pipeline_values` и `save_pipeline` по образцу `save_activity`,
  вызов в ветке `Ctrl+S`; проверка — тесты «пустые строки → None» и
  «save_pipeline меняет state.config и не пишет без изменений».
- [x] 1.4 Тесты `Ctrl+X` на обоих полях (`folder_pick`) и
  `apply_picked_folder` в нужное поле; проверка —
  `cargo test -p agentcli pipeline_section`.

## 2. Документация и проверка

- [x] 2.1 README: раздел «Пайплайн» в `Ctrl+P`, строка про `Ctrl+X`, список
  разделов; проверка — `grep -n "не редактируются" README.md` пуст.
- [x] 2.2 `cargo build --release`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings` проходят,
  `git diff --stat crates/core` пуст, `openspec validate
  add-pipeline-settings-section` проходит.
