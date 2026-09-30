## Why

`index-mcp` (подмодуль `mcp/index`) умеет только CLI-команды `build` и
`compare`: индекс конспектов `.docx` нельзя ни искать из чата, ни строить,
не выходя из `agentcli`. Нужен MCP-режим сервера и клиентская сторона, чтобы
пользователь выбирал каталог, файл базы, стратегию chunking, модель
эмбеддингов и остальные параметры `build` в настройках и командах, а модель
чата искала по индексу сама.

## What Changes

- `mcp/index`: подкоманда `index-mcp serve --db <файл>` — MCP-сервер на stdio
  с инструментами `index_search`, `index_status`, `index_models`,
  `index_build`, `index_compare`; `build` и `compare` остаются как были.
- `agent-cli`, ядро: поля `index_root`, `index_db`, `index_strategy`,
  `index_model`, `index_unit`, `index_chunk_size`, `index_overlap`,
  `index_max_section`, `index_min_section`, `index_ollama_url` в `Config`.
- `agent-cli`, клиент: модуль `index.rs` (процесс `index-mcp`, исполнитель
  инструментов), команды `agentcli config index set|show|clear` и
  `agentcli index build|search|status|models`, раздел «Индекс документов» в
  `Ctrl+P` с кнопкой сборки, инструменты `index_search`/`index_status`/
  `index_build` у модели в чатах и `agentcli ask`.
- README обоих репозиториев и `CLAUDE.md` зонтика описывают новое поведение.

Не меняется: cargo-зависимостей между `agent-cli` и `mcp/index` нет; в ядре
нет `clap`, `ratatui`, `rmcp`; имена и умолчания флагов `build`/`compare`;
миграции `mcp/index`; `questions.json` и отчёт сравнения.

## Capabilities

### New Capabilities
- `index-mcp-tools`: сервер `index-mcp serve` и его использование клиентом —
  инструменты индекса, настройки, команды, раздел TUI, инструменты в чатах.

### Modified Capabilities

## Impact

- `mcp/index`: `src/serve.rs`, `src/search.rs`, `src/ollama.rs`,
  `tests/protocol.rs`, `main.rs` (подкоманда, константы умолчаний, строки
  хода в stderr), зависимость `rmcp` (server, macros, transport-io).
- `agent-cli`: `crates/core/src/config.rs`; `crates/cli/src/index.rs`,
  `cli.rs`, `main.rs`, `tui.rs`, `pipeline.rs` (`expand` стал `pub(crate)`),
  `tests/index_config.rs`; `README.md`.
- Зонтик: указатели подмодулей, `CLAUDE.md`, `README.md`, `mcp/README.md`.
