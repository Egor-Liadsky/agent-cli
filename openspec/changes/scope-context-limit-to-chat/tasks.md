## 1. `ChatSettings` и `Config`

- [x] 1.1 Добавить `max_context_tokens: Option<u32>` в `ChatSettings`
      (`crates/core/src/config.rs`), сериализуемое через `#[serde(default)]`
- [x] 1.2 В `Config::default_chat_settings` копировать
      `Config.max_context_tokens` в `max_context_tokens` нового чата;
      покрыть тестом, что новый чат наследует умолчание, а изменение
      `Config.max_context_tokens` не трогает уже собранный `ChatSettings`
- [x] 1.3 Убедиться, что старый конфиг/старые сохранённые настройки чата без
      поля читаются как `None` (тест на разбор `ChatSettings` без поля)

## 2. `ServerAgent`: убрать отдельный параметр, брать значение из `settings`

- [x] 2.1 Убрать поле `max_context_tokens` и параметр из `ServerAgent::new`
      (`crates/client/src/lib.rs`); `build_body` берёт значение из
      `settings.max_context_tokens` вместо `self.max_context_tokens`
- [x] 2.2 Обновить `CliAgent::from_config` (`crates/cli/src/agent.rs`):
      убрать проброс `config.max_context_tokens` в `ServerAgent::new`
- [x] 2.3 Обновить существующие тесты `crates/client/src/tests.rs` под новую
      сигнатуру `ServerAgent::new`; переписать
      `max_context_tokens_is_sent_when_configured` и
      `..._in_ask_in_chat_when_configured` так, чтобы значение приходило
      через `ChatSettings`, а не конструктор — тело `POST /v1/chat`
      по-прежнему содержит поле, когда оно задано в `settings`, и не
      содержит, когда не задано (для `ask` и `ask_in_chat`)
- [x] 2.4 Добавить `max_context_tokens: Option<u32>` в `ChatSettingsUpdate` и
      `settings_payload` (`crates/client/src/chats.rs`), сериализуется
      всегда (включая `null`), как поля сэмплирования — `PATCH
      /v1/chats/{id}` должен уметь явно снять лимит чата, а не только
      задать; покрыть тестом в `crates/client/src/chats/tests.rs`, что
      `null` уходит при пустом значении и число — при заданном

## 3. Команда `config context-limit`: умолчание для новых чатов

- [x] 3.1 Обновить доку команды `ConfigAction::ContextLimit`
      (`crates/cli/src/cli.rs`) и обработчик `run_context_limit_action`
      (`crates/cli/src/main.rs`): текст подтверждения и `--help` явно
      называют значение умолчанием для новых чатов (не «лимитом для всех
      чатов сейчас»); логика `set`/`clear`/`show`/отказ на `0` не меняется
      — интеграционный тест `crates/cli/tests/context_limit.rs` остаётся
      верным без изменений в проверках, кроме, возможно, текста вывода

## 4. TUI: поле лимита переезжает к настройкам чата

- [x] 4.1 В `SettingsEditor::from_chat` (`crates/cli/src/tui.rs`)
      инициализировать `context_limit` из
      `chat.settings.max_context_tokens`, а не из `Config`
- [x] 4.2 В обработчике `Ctrl+S` включить `max_context_tokens` (результат
      `editor.build_context_limit()`) в `ChatSettings`, передаваемый в
      `request_update_chat`, и убрать его из `save_connection` (сигнатура
      `save_connection` теряет параметр `max_context_tokens`)
- [x] 4.3 Проверить вручную (`cargo run -p agentcli -- chat`): у двух разных
      чатов задать разные значения лимита, убедиться по журналу запросов
      (`requests.jsonl`), что каждый чат шлёт своё значение
      `settings.max_context_tokens`

## 5. Документация и проверка

- [x] 5.1 Обновить README `agent-cli`: раздел «Модель, сервис и токен» —
      лимит контекста больше не в списке общих полей подключения, а рядом с
      описанием поля «Модель» как настройка конкретного чата; команда
      `config context-limit` описана как умолчание для новых чатов
- [x] 5.2 Прогнать `cargo test` в `agent-cli` целиком, убедиться, что ничего
      не сломано
