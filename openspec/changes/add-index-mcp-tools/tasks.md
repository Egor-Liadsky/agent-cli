## 1. Сервер `mcp/index`

- [x] 1.1 Умолчания `build` — константы в `defaults`, их берут флаги clap и
  инструмент; строки хода сборки в stderr; проверка —
  `cargo test cli_defaults_match_spec`.
- [x] 1.2 `search.rs` (перебор косинусом, сверка модели), `ollama.rs`
  (модели с `embedding`, `embedding_length`), `Embedder::probe_dim`,
  `Store::build_times`; проверка — `cargo test`.
- [x] 1.3 `serve.rs` и подкоманда `serve`, зависимость `rmcp`; проверка —
  `cargo build --release`.
- [x] 1.4 `tests/protocol.rs` против собранного бинарника с Ollama на
  `wiremock`; проверка — `cargo test --test protocol`.
- [x] 1.5 README `mcp/index` (`serve`, контракт, «Решения»); коммит и пуш в
  `origin main`.

## 2. Клиент `agent-cli`

- [x] 2.1 Поля `index_*` и `Config::index_active` в ядре; проверка —
  `cargo test -p agentcore index_tools`.
- [x] 2.2 `crates/cli/src/index.rs`: запуск сервера, `ToolExecutor`,
  `is_write`, живой тест `live_index` под `#[ignore]`; проверка —
  `cargo test -p agentcli index::`.
- [x] 2.3 Команды `config index` и `index build|search|status|models`;
  проверка — `cargo test -p agentcli --test index_config`.
- [x] 2.4 Раздел «Индекс документов» в TUI с кнопкой сборки; проверка —
  `cargo test -p agentcli index_`.
- [x] 2.5 Инструменты индекса в `ToolSet` чата и `ask`; проверка — ручной
  прогон и `cargo build --release`.
- [x] 2.6 README `agent-cli`; коммит и пуш в `origin main`.

## 3. Зонтик и проверка

- [x] 3.1 Указатели подмодулей, раздел «Связь `agentcli` ↔ `index-mcp`» в
  `CLAUDE.md`, строки в `README.md` и `mcp/README.md`.
- [x] 3.2 `cargo build --release`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings` в обоих репозиториях;
  `live_index` и `live_ollama` против Ollama; ручной прогон
  `config index set` → `index build` → `index search`.
