## 1. Конфигурация

- [x] 1.1 Добавить `max_context_tokens: Option<u32>` в `Config`
      (`crates/core/src/config.rs`), сериализуемое через `#[serde(default)]`;
      проверить тестом, что старый конфиг без поля читается как `None`
- [x] 1.2 Задокументировать поле в README `agent-cli` рядом с описанием
      конфигурации клиента

## 2. Передача в запрос к сервису

- [x] 2.1 Добавить `max_context_tokens: Option<u32>` в `ServerAgent`
      (`crates/client/src/lib.rs`), принять его в `ServerAgent::new` как ещё
      один параметр (по образцу `model`), и в `ChatSettingsPayload` —
      необязательное поле с `skip_serializing_if = "Option::is_none"`
- [x] 2.2 Заполнять поле `ChatSettingsPayload.max_context_tokens` в
      `build_body` из `self.max_context_tokens`; покрыть модульным тестом,
      что тело `POST /v1/chat` содержит поле, когда оно задано, и не
      содержит, когда не задано (для `ask` и `ask_in_chat`)
- [x] 2.3 Прокинуть `config.max_context_tokens` в `ServerAgent::new` из
      `CliAgent::from_config` (`crates/cli/src/agent.rs`)

## 3. Команда `config context-limit`

- [x] 3.1 Добавить `ConfigAction::ContextLimit { action: ContextLimitAction }`
      с действиями `Set { tokens: u32 }`, `Clear`, `Show` в
      `crates/cli/src/cli.rs`, по образцу `SamplingAction`
- [x] 3.2 Реализовать обработчик команды в `crates/cli/src/main.rs`:
      `set` с нулевым или отрицательным значением (clap `u32`, поэтому
      только `0`) отклоняется понятной ошибкой без сохранения; `clear`
      снимает поле; `show` печатает текущее значение или пометку, что лимит
      не задан; `set`/`clear` сохраняют конфиг и подтверждают действие
- [x] 3.3 Проверить `set`, `clear`, `show` и отказ на `0` через
      интеграционный тест команды (или существующий способ тестирования
      `main.rs`/`cli.rs` в проекте)

## 4. Поле в TUI

- [x] 4.1 Добавить `FormatField::ContextLimit` в `crates/cli/src/tui.rs`:
      подпись, размещение в `SettingsSection::Connection`, попадание в
      `is_connection()`
- [x] 4.2 Добавить строковое поле `context_limit: String` в
      `SettingsEditor`, инициализировать его текущим значением
      `Config.max_context_tokens` при открытии панели настроек
- [x] 4.3 При сохранении настроек (`Ctrl+S`) разобрать `context_limit`:
      пусто — `None`, иначе положительное целое — `Some`, иначе ошибка в
      `editor.error` без изменения сохранённого значения; при успехе
      обновить `Config.max_context_tokens` и вызвать `Config::save()` тем
      же путём, что `server_url`/`client_token`/`ollama_url`
      (`save_connection` или отдельная функция рядом с ней), не через
      `request_update_chat`

## 5. Документация и проверка

- [x] 5.1 Обновить README `agent-cli`: команда `config context-limit`,
      поле в TUI, пример запроса с непустым лимитом в теле `POST /v1/chat`
- [x] 5.2 Прогнать `cargo test` в `agent-cli` целиком и убедиться, что
      ничего не сломано
</content>
