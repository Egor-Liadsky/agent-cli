# AGENTS.md

`agent-cli` — независимый Rust workspace (edition 2024) с `agentcore`,
`agentupstream`, `agentclient` и бинарником `agentcli`.

- Общение — на русском языке; код, команды, ошибки и commit messages — на
  английском. Документация и комментарии — на русском.
- Выполнять команды из `agent-cli`, не из зонтичного каталога.
- Перед изменениями читать локальный README и соответствующие OpenSpec-
  артефакты. Skills для Codex находятся в `.agents/skills/`, Claude-версии
  сохранены в `.claude/skills/`.
- Ядро в `crates/core` не должно получать терминальные зависимости; изменения
  ядра сначала проверяются `cargo test`.
- Общая проверка: `cargo build --release` и `cargo test`; для тестовых
  изменений применять skill `test-audit`.
- Правила `ast-index` из Claude сохранены в `.claude/rules/ast-index.md`;
  при наличии команды `ast-index` использовать её для поиска Rust-символов,
  но не считать её обязательной частью Codex runtime.
- Не удалять и не перезаписывать `.claude/` при работе с Codex.
