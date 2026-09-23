# План: git-инструменты через MCP (`mcp-server-git`)

Документ — план реализации, а не описание готового кода. Всё, что названо
«новым», в коде ещё не существует; всё, что названо «существующим», сверено
с рабочими копиями `agent-cli` (коммит `4f00b44`) и `agent-sever` (коммит
`36c0a2f`). Процесс OpenSpec к этому плану не применяется: все решения,
допущения и открытые вопросы собраны здесь.

## Контекст

### Что есть сейчас

`agent-cli` — cargo workspace из четырёх крейтов (`Cargo.toml`, секция
`[workspace] members`):

| Крейт | Путь | Роль |
|---|---|---|
| `agentcore` | `crates/core` | трейт `Agent`, `Message`, `AgentReply`, `ChatSettings`, конвейер `pipeline.rs`, локальный Ollama (`agent/ollama.rs`, `agent/local.rs`), журнал `logging.rs` |
| `agentupstream` | `crates/upstream` | облачный `POST {base_url}/chat/completions`; им пользуется только `agentd` |
| `agentclient` | `crates/client` | `ServerAgent` поверх `POST {server_url}/v1/chat` и `ChatsClient` для `/v1/chats*` |
| `agentcli` | `crates/cli` | `clap`, TUI (`tui.rs`, 7331 строка), диспетчер `CliAgent` (`agent.rs`) |

`agent-sever` — сервис `agentd` на axum. Тянет `agentcore` и `agentupstream`
git-зависимостью из ветки `main` (`agent-sever/Cargo.toml`), локально
подменяет их путями через `[patch]` в `agent-sever/.cargo/config.toml`.

Tool calling отсутствует везде:

- `Message` (`crates/core/src/agent/mod.rs`) — поля `role`, `content`,
  `reasoning`, `meta`; `Role` — `User | Assistant | System`.
- `AgentReply` — `content`, `reasoning`, `meta`, `model`, `policy`,
  `context`; поля для вызовов инструментов нет.
- `Agent::ask(&self, history: &[Message], settings: &ChatSettings)` —
  единственный метод трейта, передать описания инструментов некуда.
- `agentupstream::ChatRequest` не имеет `tools`, а
  `ChatResponseMessage.content: String` обязателен — ответ провайдера с
  `"content": null` и `tool_calls` сегодня падает в `AgentError::Decode`.
- `agent/ollama.rs::ChatRequest` не имеет `tools`, `ChatResponseMessage`
  не читает `tool_calls`.
- Контракт `/v1/chat` (`agent-sever/src/dto.rs`): `RoleDto` — только
  `User | Assistant`; `ChatResponse` без `tool_calls`; с `chat_id`
  обязателен `prompt`, `messages` запрещены (`handle_chat_in_existing` в
  `src/app.rs`).
- Хранилище: таблица `messages` (`migrations/0001_init.sql`, пересоздана в
  `0003_context_strategies.sql`) — колонки `role`, `content`, `reasoning`,
  `meta`; `store.rs::role_from_str` знает `user`, `assistant`, `system`.

### Как сегодня идёт обмен

- Облачный чат: TUI вызывает `CliAgent::ask_in_chat` →
  `ServerAgent::ask_in_chat(chat_id, prompt, settings)` → `POST /v1/chat` с
  `chat_id` и `prompt`. Сервис сам берёт историю из SQLite, собирает её
  стратегией контекста (`src/context.rs::assemble`), прогоняет `Pipeline`
  и пишет обмен `store::append_exchange` (две строки одной транзакцией).
- Локальный чат (Ollama): TUI вызывает `OllamaAgent::ask` с историей из
  памяти клиента, а обмен дозаписывает через
  `ChatsClient::append` → `POST /v1/chats/{id}/messages`
  (`request_append_exchange` в `tui.rs`).
- `agentcli ask`: `run_ask` в `crates/cli/src/main.rs` зовёт
  `CliAgent::ask` без чата — облако через `POST /v1/chat` с `messages`,
  Ollama напрямую.

Настройки чата (`ChatSettings`) хранит сервис: колонка `chats.settings` —
JSON `ChatSettings` (`store.rs::parse_settings`, при ошибке разбора —
умолчания сервиса), изменения идут через `PATCH /v1/chats/{id}` с телом
`ChatSettingsDto`, у которого `#[serde(deny_unknown_fields)]`.

### Внешние источники

- **`mcp-server-git`** (README в `modelcontextprotocol/servers`,
  `src/git`): запуск `uvx mcp-server-git`, опция `--repository <путь>`.
  Двенадцать инструментов, у каждого обязательный аргумент `repo_path`:

  | Инструмент | Аргументы кроме `repo_path` | Класс |
  |---|---|---|
  | `git_status` | — | читающий |
  | `git_diff_unstaged` | `context_lines?` | читающий |
  | `git_diff_staged` | `context_lines?` | читающий |
  | `git_diff` | `target`, `context_lines?` | читающий |
  | `git_log` | `max_count?`, `start_timestamp?`, `end_timestamp?` | читающий |
  | `git_show` | `revision` | читающий |
  | `git_branch` | `branch_type`, `contains?`, `not_contains?` | читающий |
  | `git_add` | `files` | пишущий (индекс) |
  | `git_reset` | — | пишущий (индекс) |
  | `git_commit` | `message` | пишущий (история) |
  | `git_create_branch` | `branch_name`, `base_branch?` | пишущий (ссылки) |
  | `git_checkout` | `branch_name` | пишущий (рабочая копия) |

- **Спецификация MCP** (modelcontextprotocol.io, раздел Server → Tools):
  `tools/list` с пагинацией (`cursor`/`nextCursor`), описание инструмента —
  `name`, `description`, `inputSchema` (JSON Schema), `annotations`;
  `tools/call` возвращает `content[]` (элементы `text`, `image`, `audio`,
  `resource_link`, `resource`) и `isError`. Ошибки двух видов: протокольные
  (JSON-RPC `error`, например неизвестный инструмент) и ошибки выполнения
  (`isError: true`). Спецификация требует считать `annotations` недоверенными
  и рекомендует человека в контуре: подтверждение чувствительных операций,
  показ аргументов до вызова, таймауты, аудит вызовов.
- **Ollama `/api/chat`** (`docs/api.md` в репозитории ollama): запрос с
  `tools: [{ "type": "function", "function": { name, description,
  parameters } }]`; ответ — `message.tool_calls: [{ "function": { name,
  arguments } }]`, где `arguments` — объект, а `id` в документации нет;
  результат возвращается сообщением `{ "role": "tool", "content",
  "tool_name" }`.

### Выбор SDK для MCP

Взять официальный крейт `rmcp` (репозиторий
`modelcontextprotocol/rust-sdk`, на crates.io последняя версия `3.4.0` от
15.09.2026, MSRV 1.88, лицензия Apache-2.0). Подключение:

```toml
rmcp = { version = "3.4", default-features = false, features = ["client", "transport-child-process"] }
```

Умолчания крейта (`base64`, `macros`, `server`) не нужны: клиенту не нужен
серверный код и макросы.

Отвергнуто — собственный JSON-RPC поверх stdio. Протокол небольшой, но
рукописная реализация обязана повторить рукопожатие `initialize` /
`notifications/initialized` с согласованием версии протокола, пагинацию,
отмену (`notifications/cancelled`), разбор всех типов `content` и
сопоставление ответов по `id`. Это код без собственной ценности, который
придётся догонять при каждой ревизии спецификации; `rmcp` уже ведёт
ревизии и выпускается той же организацией, что и спецификация.

## Решения

### (а) Типы ядра и совместимость старых данных

**Новый модуль `crates/core/src/agent/tools.rs`** — только данные, без
зависимостей сверх уже имеющихся (`serde`, `serde_json`):

```rust
/// Описание инструмента, которое уходит модели.
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema аргументов (для MCP — `inputSchema`).
    pub parameters: serde_json::Value,
}

/// Вызов инструмента, который вернула модель.
pub struct ToolCall {
    /// Идентификатор вызова. У OpenAI-совместимых провайдеров приходит от
    /// провайдера; у Ollama его нет — назначает ядро (`call_<n>`).
    pub id: String,
    pub name: String,
    /// Аргументы как JSON-объект. OpenAI-формат передаёт их строкой —
    /// перевод в объект и обратно делает `agentupstream`.
    pub arguments: serde_json::Value,
}

/// Закрывает «висячие» вызовы: для каждого `tool_calls` без ответа роли
/// `tool` добавляет синтетический результат, а ответы `tool` без вызова
/// выбрасывает. Нужна обоим провайдерам и сервису (см. решение «д»).
pub fn close_dangling_tool_calls(history: &[Message]) -> Vec<Message>;
```

Все три типа — `Debug, Clone, PartialEq, Serialize, Deserialize`.

**Изменения `crates/core/src/agent/mod.rs`:**

- `Role` получает вариант `Tool` (`"tool"` в JSON через уже стоящий
  `rename_all = "lowercase"`).
- `Message` получает поля, все с `#[serde(default, skip_serializing_if =
  …)]`:
  - `tool_calls: Vec<ToolCall>` — у ответа модели, запросившего
    инструменты;
  - `tool_call_id: Option<String>` — у сообщения роли `tool`;
  - `tool_name: Option<String>` — у сообщения роли `tool` (нужно Ollama,
    который сопоставляет результат по имени, а не по `id`).
- Конструкторы `Message::assistant_with_tool_calls(content, calls)` и
  `Message::tool_result(call_id, name, content)`. Существующие
  конструкторы заполняют новые поля пустыми значениями.
- `AgentReply` получает `tool_calls: Vec<ToolCall>`. Пустой вектор значит
  «ответ окончательный».
- Трейт `Agent` получает второй метод с реализацией по умолчанию:

  ```rust
  async fn ask_with_tools(
      &self,
      history: &[Message],
      settings: &ChatSettings,
      tools: &[ToolSpec],
  ) -> Result<AgentReply> {
      if tools.is_empty() {
          return self.ask(history, settings).await;
      }
      Err(AgentError::ToolsUnsupported { model: None, request_id: None }.into())
  }
  ```

  Переопределяют его `OllamaAgent`, `UpstreamAgent`, `ServerAgent`, а
  также диспетчеры `CliAgent` (`crates/cli/src/agent.rs`) и `ServiceAgent`
  (`agent-sever/src/state.rs`).

Отвергнуто:

- *Добавить `tools` параметром в `ask`.* Ломает каждую реализацию `Agent`,
  включая тестовые подделки (`FakeAgent` в `pipeline.rs`, подделки в
  `invariants.rs` и `agent-sever/src/tests.rs`) и судью, которому
  инструменты не нужны. Метод по умолчанию сохраняет правило CLAUDE.md
  «новый провайдер — реализацией трейта»: провайдер без инструментов не
  пишет ни строчки.
- *Передавать инструменты полем `ChatSettings`.* `ChatSettings`
  сериализуется в файл чата на сервисе и в DTO `/v1/chats`; описания
  инструментов — свойство конкретного запуска MCP-сервера, а не чата.
- *Хранить `tool_calls` внутри `content` (как текст).* Модель теряет
  структуру вызова, провайдеры требуют отдельное поле.

**Совместимость старых данных.** Все новые поля `Message` помечены
`serde(default)`, поэтому старые JSON-сообщения (журналы, ответы сервиса)
читаются без ошибки. Старые записи БД: новые колонки добавляются
nullable-миграцией (решение «б»), `NULL` читается как пустой вектор /
`None`. `ChatSettings` получает новые поля тоже с `serde(default)` —
существующие `chats.settings` разбираются как раньше, а не падают в ветку
«использованы умолчания сервиса» из `store.rs::parse_settings`. На каждое
новое поле — тест вида `old_chat_settings_without_…_parse_as_none`, как
существующие в `config.rs`.

**Новые поля `ChatSettings`** (`crates/core/src/config.rs`), по образцу
`max_context_tokens` — поле в чате плюс умолчание в `Config`,
копируемое в `default_chat_settings`:

| Поле | Тип | Смысл |
|---|---|---|
| `git_tools_enabled` | `Option<bool>` | `None`/`false` — инструменты не подключаются |
| `git_repository` | `Option<String>` | путь к репозиторию для `--repository` |
| `git_allowed_tools` | `Option<Vec<String>>` | `None` — только читающие инструменты; пишущие попадают к модели, лишь если перечислены явно |
| `tool_max_iterations` | `Option<u32>` | лимит итераций цикла, `None` — 8, потолок 32 |

Сервис эти поля не использует, но обязан их хранить: иначе настройка,
сохранённая через `PATCH /v1/chats/{id}`, не переживёт перезагрузку
списка чатов. Поэтому они добавляются в `ChatSettingsDto` (`dto.rs`, та же
семантика `double_option`, что у `profile_id`) и в `ChatSettingsUpdate`
(`crates/client/src/chats.rs`). В разовый `ChatSettingsPayload`
(`crates/client/src/lib.rs`) они не попадают: вызову модели они не нужны.

**Новые варианты `AgentError`** (`crates/core/src/agent/error.rs`):

| Вариант | Где возникает | Как различается потребителем |
|---|---|---|
| `ToolsUnsupported { model: Option<String>, request_id: Option<String> }` | Ollama отвечает, что модель не умеет tools; `agentd` — код `tools_unsupported`; реализация по умолчанию `ask_with_tools` | TUI подсказывает выбрать другую модель или выключить git-инструменты |
| `ToolServerUnavailable { server: String, reason: String }` | `crates/cli`: нет `uvx`, процесс не стартовал, не прошёл `initialize`, упал и не перезапустился | TUI подсказывает установить `uv` или проверить путь |
| `ToolLoopLimit { iterations: u32 }` | цикл инструментов: модель продолжает звать инструменты после принудительного финального запроса | TUI показывает, сколько итераций прошло |

`request_id()` и `Display` дополняются; `agent-sever/src/error.rs::from_agent_error`
получает ветки (`ToolsUnsupported` → `400 tools_unsupported`, два других на
сервисе не возникают → `internal`, как сейчас `MissingApiKey`).

Единственное место, где причина берётся из текста, — граница провайдера
Ollama: `parse_error` в `agent/ollama.rs` превращает `400` с текстом
`does not support tools` в `ToolsUnsupported`. Дальше по стеку причина
различается только `downcast_ref`. Клиент (`parse_service_error` в
`crates/client/src/lib.rs`) переводит в `ToolsUnsupported` машинный
`code` конверта, а не текст `message`.

### (б) Контракт `/v1/chat`

**Запрос** получает два необязательных поля верхнего уровня (не в
`settings`: инструменты не сохраняются в чате):

```json
{
  "chat_id": "…",
  "prompt": "Что изменилось в рабочей копии?",
  "tools": [
    { "name": "git_status",
      "description": "Shows the working tree status",
      "parameters": { "type": "object", "properties": {}, "required": [] } }
  ]
}
```

Продолжение хода в чате — `tool_results` вместо `prompt`:

```json
{
  "chat_id": "…",
  "tool_results": [
    { "tool_call_id": "call_0", "name": "git_status", "content": "On branch main…" }
  ],
  "tools": [ … тот же список … ]
}
```

Правила разбора (`app.rs`, новые проверки рядом с `history_from`):

- С `chat_id` — ровно одно из `prompt` и `tool_results`, иначе `400
  invalid_request`. `messages` с `chat_id` по-прежнему `400`.
- `tool_results` принимаются, только если последнее сообщение активной
  ветки — ответ модели с `tool_calls`, и множество `tool_call_id`
  совпадает с множеством `id` этих вызовов. Иначе `400
  tool_results_mismatch`.
- Без `chat_id` — `messages` могут содержать роль `tool`
  (`tool_call_id`, `tool_name`) и ответы модели с `tool_calls`.
  `RoleDto` получает `Tool`, `MessageDto` — три поля из решения «а».
  Результат, не ссылающийся на вызов предыдущего ответа модели, — `400
  tool_results_mismatch`.
- `tools`: не больше 128 элементов, имя по `^[A-Za-z0-9_-]{1,64}$`, имена
  уникальны, `parameters` — JSON-объект. Иначе `400 tools_invalid`.
  Пустой список равносилен отсутствию поля.

**Ответ** `ChatResponse` получает `tool_calls` — массив, присутствует
всегда (пустой у окончательного ответа), `content` у промежуточного
ответа может быть пустой строкой:

```json
{
  "request_id": "…", "content": "", "reasoning": null, "model": "deepseek-v4-flash",
  "tool_calls": [ { "id": "call_0", "name": "git_status", "arguments": {} } ],
  "usage": { … }, "timing": { … }, "policy": { … },
  "chat_id": "…", "seq": 2, "context": { … }
}
```

Клиент читает поле с `serde(default)`, поэтому новый клиент работает и со
старым сервисом (инструментов просто не будет), а старый клиент — с новым
сервисом (лишнее поле игнорируется).

**Ошибки** — только единым конвертом
`{ "error": { "code", "message", "request_id" } }` и заголовком
`x-request-id`, через существующий `ApiError`. Новые коды: `tools_invalid`
(400), `tool_results_mismatch` (400), `tools_unsupported` (400).

**Хранение в чате.** Ход с инструментами пишется частями, каждая —
существующей транзакцией `store::append_messages`:

1. запрос с `prompt`, ответ с `tool_calls` → `user` + `assistant`
   (с `tool_calls`), как сегодня `append_exchange`;
2. запрос с `tool_results`, любой ответ → все `tool` + `assistant`.

Промежуточные сообщения пишутся сразу, а не в конце хода: пишущий
инструмент мог изменить репозиторий, и история чата обязана это
отражать, даже если клиент потом упадёт.

**Миграция** — новый файл `agent-sever/migrations/0007_tool_calls.sql`:

```sql
ALTER TABLE messages ADD COLUMN tool_calls   TEXT;  -- JSON Vec<ToolCall>
ALTER TABLE messages ADD COLUMN tool_call_id TEXT;
ALTER TABLE messages ADD COLUMN tool_name    TEXT;
```

`store.rs`: `role_to_str`/`role_from_str` получают `"tool"`;
`ChatMessage` и `NewMessage` — три поля; `insert_message` и все три
`SELECT seq, role, content, reasoning, meta, created_at` читают и пишут
новые колонки. `MessageView` (`GET /v1/chats/{id}`) и `NewMessageDto`
(`POST /v1/chats/{id}/messages`) отдают и принимают их — иначе локальный
чат Ollama не сможет сохранить ход, а перезагрузка истории потеряет
вызовы.

Отвергнуто:

- *Хранить `tool_calls` в JSON-колонке `meta`.* `meta` — телеметрия
  (`MessageMeta`), `MessageView` отдаёт её только для ответов модели;
  смешение значений ломает смысл колонки.
- *Держать промежуточные сообщения только у клиента и присылать их
  каждый раз в `messages`.* С `chat_id` контракт `messages` запрещает
  намеренно (история принадлежит сервису), а стратегии контекста
  работают по сохранённым сообщениям.
- *Выполнять инструменты на сервисе.* Запрещено границами задачи: `agentd`
  не запускает процессы и не видит файловую систему пользователя.

### (в) Конвейер и стратегии контекста при ответе с одними `tool_calls`

**`pipeline.rs`:**

- `RequestContext` получает `tools: Vec<ToolSpec>` и `with_tools`.
  Основной вызов в `Pipeline::run` идёт через `ask_with_tools`.
- `normalize` сейчас выбрасывает сообщения с пустым `content` — это
  уничтожило бы ответ модели, состоящий из одних `tool_calls`. Новое
  правило: сообщение остаётся, если непуст `content`, или непуст
  `tool_calls`, или роль `Tool`.
- Выходные политики выполняются на каждом ответе, включая промежуточный:
  политика получает весь `AgentReply` и может проверить `tool_calls`.
- Судья вызывается только для окончательного ответа (`tool_calls`
  пуст): оценивать нечего, пока модель собирает данные.

**`InvariantGuard`** (`crates/core/src/invariants.rs`): при пустом
`content` возвращает `Pass` без служебного вызова модели. Иначе каждая
итерация цикла стоила бы лишнего запроса к модели ради пустого текста.
Аргументы пишущих вызовов вместо этого проверяет человек (решение «е»).

**Стратегии контекста `agentd`:**

- Сообщения `tool` и ответы с `tool_calls` хранятся как обычные сообщения
  и проходят через стратегии без особых правил. `summary::tail_boundary`
  уже сдвигает границу хвоста к сообщению пользователя, поэтому группа
  «вызов → результаты» внутри хода не разрывается на границе окна.
- Пять копий `message_from_stored` (`app.rs`, `window.rs`, `summary.rs`,
  `facts.rs`, `branch.rs`) переносят три новых поля.
- `summary::role_label` получает «Инструмент»; в запрос пересказа текст
  сообщений `tool` попадает обрезанным до 2 000 символов: вывод
  `git_diff` иначе вытеснит всё остальное.
- `app.rs::estimate_tokens` и `role_str` учитывают роль `tool` и длину
  сериализованных `tool_calls`.
- Перед вызовом модели история проходит `close_dangling_tool_calls`:
  висячий вызов остаётся, если клиент упал посреди хода или ветка
  (`POST /v1/chats/{id}/branches`) создана от середины хода.
- Фоновые задачи после обмена — факты (`facts::update_after_exchange`),
  маршрутизатор памяти, трекер задачи, автоназвание — запускаются только
  после окончательного ответа. Их вход `prompt` — последнее сообщение
  пользователя в истории, а не `tool_results`. Автоназвание по-прежнему
  привязано к `user_row.seq` первого запроса хода.

Отвергнуто — *исключать сообщения инструментов из окна `sliding_window`*:
окно считается в сообщениях, но выброс результатов оставил бы в истории
вызовы без ответов, что провайдер отвергает. Цена записана в рисках.

### (г) Жизненный цикл процесса `mcp-server-git`

Новый модуль `crates/cli/src/mcp.rs`, тип `GitToolServer`:

- **Запуск — лениво**, при первой отправке реплики в чате с
  `git_tools_enabled = Some(true)`, а не при старте TUI: большинство чатов
  инструменты не используют. Команда
  `uvx mcp-server-git --repository <git_repository>` через
  `tokio::process::Command` и транспорт `rmcp` для дочернего процесса;
  `kill_on_drop(true)`; stderr процесса — в `Stdio::null()`, чтобы не
  рисовать поверх TUI. Затем рукопожатие и `tools/list` со всеми
  страницами. Таймаут всего запуска — 60 секунд: первый `uvx` скачивает
  пакет.
- **Проверка пути** до запуска: каталог существует и содержит `.git`
  (файл или каталог). Иначе `ToolServerUnavailable` с понятной причиной,
  процесс не запускается.
- **Нет `uvx`**: ошибка запуска с `io::ErrorKind::NotFound` →
  `ToolServerUnavailable { server: "mcp-server-git", reason: "не найден
  uvx: установите uv …" }`. Ход не выполняется, реплика пользователя
  остаётся в поле ввода.
- **Один процесс на репозиторий.** `AppState` держит
  `HashMap<PathBuf, Arc<GitToolServer>>` с каноническим путём в ключе:
  чаты с одним репозиторием делят процесс.
- **Таймаут вызова** — 30 секунд. По истечении запрос отменяется, модели
  уходит результат-ошибка «вызов не уложился в 30 с», а процесс
  перезапускается перед следующим вызовом: состояние зависшего сервера
  неизвестно.
- **Падение процесса** (транспорт закрыт): одна попытка перезапуска в
  пределах хода, текущий вызов получает результат-ошибку. Неудачный
  перезапуск прерывает ход ошибкой `ToolServerUnavailable`.
- **Остановка**: при выходе из TUI (после `run_app` в `tui::run`), при
  выключении инструментов или смене пути в настройках чата, если
  репозиторий больше не нужен ни одному чату. Сначала штатная отмена
  сервиса `rmcp` с ожиданием до 2 секунд, затем `kill`. В режиме `ask`
  процесс живёт до конца команды.
- **Список инструментов фиксируется на время жизни процесса.**
  Уведомление `notifications/tools/list_changed` игнорируется (у
  `mcp-server-git` набор статичный).

**Подготовка описаний для модели:**

- фильтр по `git_allowed_tools`; при `None` остаются только читающие;
- из `inputSchema` удаляется свойство `repo_path` (и из `required`):
  клиент сам подставляет в аргументы настроенный путь, перезаписывая
  значение модели. Модель не может обратиться к другому репозиторию,
  даже если сервер этого не запрещает;
- `description` берётся как есть, `annotations` не используются
  (спецификация велит не доверять им).

**Результат вызова для модели:** текстовые элементы `content` склеиваются
через перевод строки; прочие типы заменяются пометкой `[<тип> опущен]`;
при `isError: true` текст начинается с `Ошибка инструмента: `.
Протокольная ошибка JSON-RPC (неизвестный инструмент, неверные
аргументы) тоже становится результатом-ошибкой, а не ошибкой хода.
Результат длиннее 16 000 символов обрезается с пометкой о числе
отброшенных символов.

Отвергнуто:

- *Запускать сервер при старте TUI.* Лишний процесс и сетевая загрузка
  `uvx` для пользователя, которому git не нужен.
- *Процесс на каждый вызов.* Рукопожатие и запуск Python на каждый
  `git_status` — секунды задержки.
- *Прерывать ход по таймауту вызова.* Модель умеет обработать ошибку
  инструмента (повторить, ответить без данных); прерывание оставило бы
  пользователя без ответа.

### (д) Цикл инструментов и лимит итераций

Новый модуль `crates/cli/src/tool_loop.rs`, функция уровня
`run_tool_loop`. Зависимости передаются трейтами, чтобы цикл
тестировался без процесса и сети:

```rust
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    fn is_write(&self, name: &str) -> bool;
    async fn call(&self, call: &ToolCall) -> Result<String>;   // текст для модели
}

#[async_trait]
pub trait ToolApprover: Send + Sync {
    async fn approve(&self, call: &ToolCall) -> bool;
}
```

Порядок одной итерации:

1. Запрос к модели с `tools`:
   - облачный чат — `ServerAgent::ask_in_chat` (первая итерация, с
     `prompt`) и новый `ServerAgent::continue_in_chat(chat_id,
     tool_results, settings, tools)` (последующие);
   - локальный чат и `agentcli ask` — `ask_with_tools` с историей,
     которую цикл наращивает сам.
2. `tool_calls` пуст — ответ окончательный, цикл завершён.
3. Иначе вызовы выполняются последовательно, в порядке ответа, не больше
   16 за итерацию (лишние получают результат-ошибку). Для каждого:
   - имя не в отфильтрованном списке → результат «инструмент не
     разрешён», вызов не выполняется;
   - пишущий → `ToolApprover::approve`; отказ → результат «пользователь
     отклонил вызов»;
   - иначе `ToolExecutor::call` с подставленным `repo_path`.
4. Результаты становятся сообщениями роли `tool`, счётчик итераций
   растёт, переход к шагу 1.

Параллельного выполнения нет: `mcp-server-git` работает с одной рабочей
копией, а порядок `git_add` → `git_commit` важен.

**Лимит.** Итерация — один ответ модели с непустым `tool_calls`. Лимит —
`ChatSettings::tool_max_iterations`, по умолчанию 8, потолок 32. На
исчерпании невыполненные вызовы получают результат «лимит вызовов
инструментов исчерпан, ответь по уже полученным данным», и делается один
финальный запрос с пустым `tools`. Если модель и тогда вернула
`tool_calls` — ход завершается `AgentError::ToolLoopLimit`.

**Запись хода:**

- облачный чат — сервис пишет сам (решение «б»);
- локальный чат — после завершения хода клиент дозаписывает все
  сообщения хода одним `ChatsClient::append`. Если ход прервался ошибкой
  после хотя бы одного выполненного вызова, дозаписывается выполненная
  часть (`user`, вызовы, результаты), закрытая `close_dangling_tool_calls`:
  побочные эффекты уже произошли. Ход без выполненных вызовов при ошибке
  не пишется — как сейчас.

**Отображение в TUI:** промежуточные сообщения добавляются в историю
чата по мере прихода новым событием `ChatEvent::ToolTurnMessages(chat_id,
Vec<Message>)`; строка ожидания показывает текущий инструмент. Сообщение
роли `tool` рисуется свёрнуто: имя инструмента и число строк результата;
полный текст доступен через существующий режим выбора сообщения
(`Ctrl+G`). Окончательный ответ приходит прежним `ChatEvent::Response`.

Отвергнуто:

- *Цикл внутри `agentcore`.* Ядру пришлось бы знать об исполнителе
  инструментов и подтверждении, то есть о процессе и интерфейсе
  пользователя.
- *Цикл на сервисе с обратным вызовом клиента.* Требует
  двунаправленного канала (WebSocket/SSE) и держит HTTP-запрос открытым,
  пока человек думает над подтверждением.
- *Жёсткая остановка на лимите без финального запроса.* Пользователь
  остаётся без ответа, хотя данные уже собраны.

### (е) Подтверждение пишущих вызовов и отказ в `ask`

**Классификация** в `mcp.rs`: явный список читающих
(`git_status`, `git_diff_unstaged`, `git_diff_staged`, `git_diff`,
`git_log`, `git_show`, `git_branch`). Всё остальное, включая
`git_add`, `git_commit`, `git_reset`, `git_checkout`,
`git_create_branch` и любой инструмент будущих версий сервера, —
пишущее. Неизвестное считается опасным, а не безопасным.

**TUI:**

- `ToolApprover` для TUI отправляет `ChatEvent::ToolApproval { chat_id,
  call, reply: oneshot::Sender<bool> }` и ждёт ответа.
- Новый `Focus::ToolConfirm` и попап по образцу `render_delete_popup` /
  `handle_confirm_key`: название чата, имя инструмента, аргументы
  целиком и отформатированно (полное сообщение коммита, список файлов,
  имя ветки). `y`/`д`/`Enter` — выполнить, `n`/`н`/`Esc` — отклонить.
  Режима «разрешить всё до конца хода» нет: каждый пишущий вызов виден
  отдельно.
- Запросы из разных чатов встают в очередь (`VecDeque` в `AppState`) и
  показываются по одному.
- Выход из TUI при открытом запросе: отправитель уничтожается, цикл
  получает отказ и завершает ход; процесс сервера останавливается
  общим порядком.

**`agentcli ask`:** `ToolApprover`, который всегда отказывает. Модель
получает результат «пишущие инструменты в режиме ask не выполняются:
используйте agentcli chat», в stderr печатается предупреждение с именем
отклонённого инструмента. Флага, разрешающего запись без подтверждения,
нет.

Отвергнуто:

- *Доверять `annotations.readOnlyHint`.* Спецификация прямо называет
  аннотации недоверенными.
- *Запрос подтверждения в stdin для `ask`.* `ask` используется в
  скриптах и конвейерах; интерактивный вопрос повесил бы их.

### (ж) Журнал `requests.jsonl` и `responses.jsonl`

- Вызов MCP пишется тем же `ExchangeLog`, что и обмен с моделью:
  `RequestLogEntry { url: "mcp+stdio://mcp-server-git/tools/call",
  model: <имя инструмента>, request: { "name", "arguments" } }` и
  `ResponseLogEntry { status, duration_ms, response }`, где `status` —
  200 для успеха, 500 для `isError`/протокольной ошибки, 504 для
  таймаута. Запуск сервера пишется одной записью со списком имён
  инструментов (без схем).
- Новая функция `agentcore::logging::redact_secrets(&serde_json::Value)
  -> serde_json::Value` маскирует строки, похожие на секреты: префиксы
  ключей (`sk-`, `ghp_`, `github_pat_`, `xox`, `AKIA`), `Bearer <…>`,
  значения после `password=`, `token=`, `secret=`, `api_key=` в строках
  диффов. Формат маски — как у `mask` в `agent-sever/src/config.rs`
  (`secr***alue`). Вызывается в `ExchangeLog::send` для всех записей:
  результаты инструментов попадают и в тела запросов к модели
  (`requests.jsonl` у `ServerAgent` и `ollama.rs`), и одно место
  маскирования закрывает оба пути.
- В журнал результат инструмента пишется обрезанным до 4 000 символов.
- Сервис: содержимое `tool_results` считается содержимым запроса и
  журналируется только при `AGENTD_LOG_CONTENT=true`; отладочный приёмник
  (`TracingExchangeSink`) проходит через тот же `ExchangeLog::send` и
  получает маскирование автоматически.

Отвергнуто — *отдельный файл `tools.jsonl`*: CLAUDE.md фиксирует два
файла журнала, а связь «вызов модели → вызов инструмента» проще
восстанавливать в одном потоке по времени.

## Изменения по крейтам

### `agentcore` (`agent-cli/crates/core`)

| Модуль | Изменение |
|---|---|
| `src/agent/tools.rs` (новый) | `ToolSpec`, `ToolCall`, `close_dangling_tool_calls` |
| `src/agent/mod.rs` | `mod tools` и реэкспорт; `Role::Tool`; поля `Message` и конструкторы; `AgentReply::tool_calls`; `Agent::ask_with_tools` с реализацией по умолчанию |
| `src/agent/error.rs` | варианты `ToolsUnsupported`, `ToolServerUnavailable`, `ToolLoopLimit`; `request_id()`, `Display`, тесты |
| `src/agent/ollama.rs` | `tools` в `ChatRequest`; `tool_calls`/`tool_name` в `ChatMessage`; разбор `message.tool_calls` с назначением `id`; `build_messages` для роли `tool` и через `close_dangling_tool_calls`; `parse_error` → `ToolsUnsupported`; параметр `tools` у `chat` |
| `src/agent/local.rs` | `OllamaAgent::ask_with_tools` |
| `src/pipeline.rs` | `RequestContext::tools`/`with_tools`; вызов `ask_with_tools`; новое правило `normalize`; судья только для окончательного ответа |
| `src/invariants.rs` | `InvariantGuard::check`: `Pass` при пустом `content` |
| `src/config.rs` | четыре поля в `ChatSettings` и `Config`, копирование в `default_chat_settings`, тесты совместимости |
| `src/logging.rs` | `redact_secrets`, вызов в `ExchangeLog::send` |

### `agentupstream` (`agent-cli/crates/upstream`)

`src/lib.rs`:

- `ChatRequest` — `tools: Option<Vec<ToolDef>>` в формате
  `{ "type": "function", "function": { name, description, parameters } }`,
  без поля при пустом списке;
- `ChatMessage` — `content: Option<String>` (`null` у ответа с одними
  вызовами), `tool_calls` (`{ id, type: "function", function: { name,
  arguments: <строка JSON> } }`), `tool_call_id` для роли `tool`;
- `ChatResponseMessage.content` — `Option<String>` с `serde(default)`,
  плюс `tool_calls`; строка `arguments` разбирается в объект, при ошибке
  разбора сохраняется как `Value::String` — исполнитель вернёт модели
  ошибку аргументов;
- `build_messages` — роль `tool`, `tool_calls`, проход через
  `close_dangling_tool_calls`;
- `impl Agent` — `ask_with_tools`; `extract_answer` заполняет
  `AgentReply::tool_calls`.

### `agentclient` (`agent-cli/crates/client`)

- `src/lib.rs`: `ChatRequest` — `tools`, `tool_results`; `ChatMessage` —
  три поля и роль `tool`; `ChatResponse` — `tool_calls` с
  `serde(default)`; `ServerAgent::ask_with_tools`; `ask_in_chat` получает
  параметр `tools`; новый `continue_in_chat`; `parse_service_error` —
  код `tools_unsupported` → `AgentError::ToolsUnsupported`.
- `src/chats.rs`: `NewMessagePayload` и `MessagePayload` — роль `tool` и
  три поля (сейчас неизвестная роль превращается в `Assistant` —
  сообщение `tool` так показывать нельзя); `ChatSettingsUpdate` и
  `settings_payload` — четыре новых поля настроек.
- Тесты: `src/tests.rs` и `src/chats/tests.rs` на `wiremock`.

### `agentcli` (`agent-cli/crates/cli`)

| Модуль | Изменение |
|---|---|
| `Cargo.toml` | зависимость `rmcp` (см. «Выбор SDK») |
| `src/mcp.rs` (новый) | `GitToolServer`: запуск, проверка пути, `tools/list`, `tools/call` с таймаутом, перезапуск, остановка; классификация читающих/пишущих; подготовка схем (фильтр, удаление `repo_path`); журналирование вызовов |
| `src/tool_loop.rs` (новый) | трейты `ToolExecutor`, `ToolApprover`; цикл, лимит, финальный запрос; сборка сообщений хода |
| `src/agent.rs` | методы `CliAgent` для хода с инструментами по провайдеру; `ask_with_tools` |
| `src/main.rs` | `run_ask`: при включённых инструментах в умолчаниях — запуск сервера, цикл с отказывающим `ToolApprover`, остановка; `mod mcp; mod tool_loop;` |
| `src/cli.rs` | `ConfigAction::GitTools` с подкомандами `Set { enabled, --repository, --allowed-tools, --max-iterations }`, `Clear`, `Show` — по образцу `ContextLimitAction` |
| `src/tui.rs` | `SettingsSection::Tools` («Инструменты») с полями `FormatField::GitToolsEnabled`, `GitRepository`, `GitAllowedTools`, `ToolMaxIterations`; заполнение литерала `ChatSettings` в `handle_settings_key`; `ChatEvent::ToolApproval`, `ChatEvent::ToolTurnMessages`; `Focus::ToolConfirm`, попап и обработчик клавиш; реестр `GitToolServer` в `AppState` и остановка при выходе; запуск цикла в `handle_input_key` вместо прямого `ask_in_chat`; дозапись хода Ollama в обработке `ChatEvent::Response`; отрисовка роли `Tool` |
| `src/chats.rs` | `context_block`: метка для роли `Tool` |

### `agentd` (`agent-sever`)

| Модуль | Изменение |
|---|---|
| `src/dto.rs` | `ChatRequest::tools`, `tool_results`; `RoleDto::Tool`; `MessageDto` — три поля; `ChatResponse::tool_calls`; `MessageView`, `NewMessageDto` — три поля; `ChatSettingsDto` и `apply_to` — четыре поля настроек |
| `src/app.rs` | проверка `tools` и `tool_results`; продолжение хода в `handle_chat_in_existing`; `tools` в `RequestContext` обоих обработчиков; запись промежуточных частей хода; фоновые задачи только после окончательного ответа; `estimate_tokens`, `role_str`, `message_from_stored` |
| `src/store.rs` | роль `tool`; поля `ChatMessage`/`NewMessage`; `insert_message` и чтение новых колонок |
| `migrations/0007_tool_calls.sql` (новый) | три nullable-колонки `messages` |
| `src/context.rs` | `close_dangling_tool_calls` над собранной историей в `assemble` |
| `src/window.rs`, `src/facts.rs`, `src/branch.rs` | `message_from_stored` переносит новые поля |
| `src/summary.rs` | `message_from_stored`, `role_label` для `Tool`, обрезка текста `tool` в `summary_prompt` |
| `src/error.rs` | коды `tools_invalid`, `tool_results_mismatch`, `tools_unsupported`; ветки новых `AgentError` в `from_agent_error` |
| `src/state.rs` | `ServiceAgent::ask_with_tools` с диспетчеризацией по провайдеру |
| `src/tests.rs` | сценарии контракта на `wiremock` (см. задачу 3.3) |

## Порядок работ

Каждая команда запускается из указанного каталога; пути ниже — от
`/Users/egor_lyadskiy/ai`.

### 1. Ядро и облачный провайдер (`agent-cli`)

Весь workspace компилируется целиком (`cargo test` собирает и `agentclient`,
и `agentcli`), поэтому в этой задаче `crates/client` и `crates/cli`
получают только механические правки компиляции: ветки `Role::Tool` в
исчерпывающих `match`, пустые `tool_calls` в литералах `Message` /
`AgentReply`, новые поля в литералах `ChatSettings` (`tui.rs`,
`handle_settings_key`). Функциональность клиента — в задаче 4.

1.1. `agent/tools.rs`, новые поля и `Role::Tool` в `agent/mod.rs`,
`ask_with_tools`. Тесты: сериализация `Message` с вызовами и без;
разбор старого JSON `Message` без новых полей; `close_dangling_tool_calls`
(висячий вызов, лишний результат, чистая история не меняется).
Итог: `agentcore` собирается, тесты зелёные.
Проверка (из `agent-cli`): `cargo test -p agentcore agent::`

1.2. Варианты `AgentError`. Тест: `downcast_ref` каждого варианта из
`anyhow::Error`, `request_id()` у `ToolsUnsupported`.
Проверка (из `agent-cli`): `cargo test -p agentcore agent::error`

1.3. Ollama с `tools`: запрос, разбор `tool_calls`, назначение `id`,
сообщение роли `tool` с `tool_name`, `ToolsUnsupported` из `400`. Тесты
на одноразовом сервере, как `stub_ollama` в `local.rs`: запрос содержит
`tools`, ответ с вызовом даёт непустой `AgentReply::tool_calls`,
`400 … does not support tools` даёт `ToolsUnsupported`.
Проверка (из `agent-cli`): `cargo test -p agentcore ollama`

1.4. `pipeline.rs` и `InvariantGuard`. Тесты: ответ из одних вызовов
переживает `normalize`; судья не вызывается при непустом `tool_calls`;
`RequestContext::tools` доходит до агента; `InvariantGuard` не зовёт
модель при пустом `content`.
Проверка (из `agent-cli`):
`cargo test -p agentcore pipeline && cargo test -p agentcore invariants`
(`cargo test` принимает один фильтр имени, поэтому два запуска)

1.5. `config.rs`: четыре поля, умолчания, тесты совместимости по образцу
`old_chat_settings_without_field_parse_as_none` и
`new_chat_inherits_default_context_limit_from_config`.
Проверка (из `agent-cli`): `cargo test -p agentcore config`

1.6. `logging.rs::redact_secrets` и вызов в `ExchangeLog::send`. Тест:
строка с `sk-…` и `password=…` в теле запроса попадает в файл журнала
замаскированной (журнал в `tempdir`; дождаться записи существующим
`ExchangeLog::shutdown`, который дожидается фонового потока).
Проверка (из `agent-cli`): `cargo test -p agentcore logging`

1.7. `agentupstream`: `tools`, `content: null`, `tool_calls` в запросе и
ответе, `arguments` строкой. Тесты на `stub_provider`: ответ
`{"content": null, "tool_calls": [...]}` разбирается; запрос содержит
`tools` и сообщение `tool` с `tool_call_id`.
Проверка (из `agent-cli`): `cargo test -p agentupstream`

1.8. Весь workspace.
Итог: всё собирается, старые тесты не сломаны.
Проверка (из `agent-cli`): `cargo test`

### 2. Публикация ядра

2.1. Коммит в `agent-cli` (английский, повелительное наклонение,
например `Add tool calling types to agentcore and upstream`) и пуш в
`main`.
Проверка (из `agent-cli`): `git push origin main && git log origin/main -1 --oneline`

2.2. Указатель подмодуля в зонтичном репозитории — отдельным коммитом.
Проверка (из `/Users/egor_lyadskiy/ai`): `git add agent-cli && git commit -m "Update agent-cli submodule for tool calling core" && git status --short`

До задачи 3 `agent-sever` продолжает собираться: его `Cargo.lock`
закреплён на прежнем коммите `agent-cli`.

### 3. Сервис (`agent-sever`)

3.1. Временно убрать `[patch]` и зафиксировать новый коммит ядра.
Проверка (из `agent-sever`):
`mv .cargo/config.toml .cargo/config.toml.off && cargo generate-lockfile && grep -A2 'name = "agentcore"' Cargo.lock`
— в выводе `source = "git+https://github.com/Egor-Liadsky/agent-cli?branch=main#<коммит из 2.1>"`.
Сейчас, с `[patch]`, строки `source` у `agentcore` в `Cargo.lock` нет
вовсе (путевая зависимость) — её появление и есть признак успеха.

3.2. Миграция `0007_tool_calls.sql`, `store.rs`, `dto.rs`, `app.rs`,
стратегии, `error.rs`, `state.rs` — по разделу «Изменения по крейтам».
Итог: сервис собирается против опубликованного ядра.
Проверка (из `agent-sever`): `cargo build --locked`

3.3. Тесты в `src/tests.rs` (провайдер — `wiremock`):
- запрос без `chat_id` с `tools` → тело к провайдеру содержит `tools`,
  ответ клиенту содержит `tool_calls`;
- `chat_id` + `prompt` → ответ с `tool_calls`, в БД `user` и `assistant`
  с `tool_calls`; затем `chat_id` + `tool_results` → в БД `tool` и
  окончательный `assistant`, фоновые задачи запущены один раз;
- `tool_results` с чужим `tool_call_id` → `400 tool_results_mismatch` в
  едином конверте с `request_id` и заголовком `x-request-id`;
- `tools` с недопустимым именем → `400 tools_invalid`;
- старая БД: строки `messages` до миграции читаются, `GET /v1/chats/{id}`
  отдаёт их без новых полей;
- `PATCH /v1/chats/{id}` с `git_repository` сохраняет поле, повторное
  чтение его возвращает;
- ответ с одними `tool_calls` проходит конвейер (не выброшен
  `normalize`).
Проверка (из `agent-sever`): `cargo test --locked`

3.4. Закоммитить код и `Cargo.lock` **пока `[patch]` ещё выключен**, затем
вернуть `[patch]`, запушить `agent-sever`, обновить указатель в корне.
Порядок важен: с `[patch]` любая локальная сборка перепишет `Cargo.lock`
на путевую зависимость, и такой лок-файл коммитить нельзя (см. «Риски»).
Проверка (из `agent-sever`):
`git show HEAD:Cargo.lock | grep -A3 'name = "agentcore"' | grep source && mv .cargo/config.toml.off .cargo/config.toml && git push origin main && git log origin/main -1 --oneline`;
затем (из `/Users/egor_lyadskiy/ai`): `git add agent-sever && git commit -m "Update agent-sever submodule for tool calling contract"`

### 4. Клиент и CLI (`agent-cli`)

4.1. `agentclient`: контракт `/v1/chat` и `/v1/chats*` с инструментами.
Тесты на `wiremock`: тело запроса с `tools` и `tool_results`; разбор
`tool_calls`; код `tools_unsupported` → `ToolsUnsupported` через
`downcast_ref`; сообщение роли `tool` из `GET /v1/chats/{id}` остаётся
ролью `Tool`; `settings_payload` содержит четыре новых поля.
Проверка (из `agent-cli`): `cargo test -p agentclient`

4.2. `tool_loop.rs` с тестами на подделках `Agent`, `ToolExecutor`,
`ToolApprover`: окончательный ответ за одну итерацию; два хода
инструментов; пишущий вызов с отказом даёт результат «отклонил» и не
вызывает исполнитель; неразрешённый инструмент не вызывается; лимит →
финальный запрос с пустым `tools`; повторные вызовы после лимита →
`ToolLoopLimit`.
Проверка (из `agent-cli`): `cargo test -p agentcli tool_loop`

4.3. `mcp.rs`: классификация (неизвестное имя — пишущее), удаление
`repo_path` из схемы, подстановка пути в аргументы, склейка `content` и
обрезка результата, отсутствие `uvx` → `ToolServerUnavailable` (запуск
несуществующей команды). Живой тест с настоящим `uvx mcp-server-git` на
временном репозитории — под `#[ignore]`.
Проверка (из `agent-cli`): `cargo test -p agentcli mcp`;
живой — `cargo test -p agentcli mcp -- --ignored`

4.4. `cli.rs`/`main.rs`: `config git-tools set|clear|show`; `ask` с
инструментами и отказом записи.
Проверка (из `agent-cli`):
`cargo test -p agentcli && cargo run -p agentcli -- config git-tools show`

4.5. `tui.rs`: раздел «Инструменты», попап подтверждения, события,
реестр серверов, остановка при выходе, дозапись хода Ollama, отрисовка
роли `tool`. Тесты в модуле `tests` файла `tui.rs`: обработчик клавиш
попапа (`y` → `true`, `Esc` → `false`), литерал настроек сохраняет четыре
поля, дозапись хода Ollama содержит все сообщения хода.
Проверка (из `agent-cli`): `cargo test -p agentcli tui`

4.6. Ручная проверка обоих провайдеров на временном репозитории
(`git init /tmp/git-mcp-demo`, один файл): облачный чат через локальный
`agentd` и чат Ollama с моделью, поддерживающей tools. Сценарий:
«покажи статус» (без подтверждения), «добавь файл и закоммить» (два
подтверждения, одно отклонение → коммита нет).
Проверка (из `agent-cli`): `cargo run -p agentcli -- chat`; итог — вывод
`git -C /tmp/git-mcp-demo log --oneline` совпадает с принятыми
подтверждениями.

4.7. Коммит, пуш `agent-cli`, указатель в корне.
Проверка (из `agent-cli`): `cargo test && git push origin main`

### 5. README обоих репозиториев

5.1. `agent-cli/README.md`: раздел «Git-инструменты (MCP)» — установка
`uv`, включение в чате (`Ctrl+P` → «Инструменты») и через
`agentcli config git-tools`, читающие и пишущие инструменты, попап
подтверждения и его клавиши, отказ записи в `ask`, лимит итераций,
журналирование и маскирование; список клавиш TUI; число разделов окна
`Ctrl+P`.
5.2. `agent-sever/README.md`: поля `tools`, `tool_results`,
`tool_calls` в `POST /v1/chat`, роль `tool` в сообщениях чата и дозаписи,
новые коды ошибок, миграция `0007`, новые поля `settings`.
Проверка (из `/Users/egor_lyadskiy/ai`):
`grep -n "tool_results\|tools_unsupported\|0007" agent-sever/README.md && grep -n "git-tools\|mcp-server-git" agent-cli/README.md`
— обе команды находят строки; затем коммиты и пуши обоих подмодулей и
указатель в корне.

## Риски и открытые вопросы

**Поддержка tools моделями.**

- Ollama: инструменты умеют не все модели. Какие модели из
  `agentcli ollama models` пользователя поддерживают tools, не проверено;
  модель по умолчанию в тесте `local.rs` — `gemma4:26b`, её поддержка
  неизвестна. Отказ Ollama распознаётся по тексту `does not support
  tools`; если текст изменится, пользователь увидит общий
  `AgentError::Provider` вместо подсказки.
- Облако: модель по умолчанию — `deepseek-v4-flash` (`DEFAULT_MODEL`).
  Поддержка function calling у неё и у `deepseek-reasoner` не проверена;
  у OpenAI-совместимых провайдеров нет единого кода ошибки «tools не
  поддерживаются», поэтому `ToolsUnsupported` для облака не выводится —
  придёт `upstream_error`.
- DeepSeek в режиме thinking может требовать возвращать
  `reasoning_content` в последующих запросах того же хода с
  инструментами. План этого не делает: `build_messages` отправляет только
  `content`. Проверить при ручной проверке 4.6; при необходимости —
  отдельная задача в `agentupstream`.

**Ollama без `id` у вызовов.** Документация не показывает `id` в
`tool_calls`; ядро назначает `call_<n>` и сопоставляет результат по
`tool_name`. Если модель вызовет один инструмент дважды за итерацию,
Ollama не различит результаты — порядок сообщений остаётся единственной
связью. Если новые версии Ollama отдают `id`, брать его.

**`rmcp` 3.4.** Точные имена API (транспорт дочернего процесса,
`serve`, постраничный `list_tools`, `call_tool`, отмена) в плане не
закреплены: сверить с docs.rs при реализации задачи 4.3. MSRV 1.88 —
убедиться, что локальный и CI-тулчейн не старше.

**`mcp-server-git`.**

- Набор инструментов взят из README и может отличаться от версии,
  которую скачает `uvx`. Неизвестные имена считаются пишущими, так что
  расхождение безопасно, но может потребовать лишних подтверждений.
- Ограничивает ли сервер `repo_path` каталогом `--repository`, из README
  не следует. План не полагается на сервер: путь подставляет клиент.
- Первый запуск `uvx` требует сети; без неё — `ToolServerUnavailable` по
  таймауту 60 секунд.

**Данные уходят провайдеру.** Результаты `git_diff`/`git_show` попадают в
историю и уходят облачному провайдеру и в SQLite `agentd`. Маскирование
действует только на журналы, не на тело запроса к модели. Фильтр
содержимого перед отправкой провайдеру сознательно отложен.

**Путь репозитория хранится на сервисе.** `git_repository` — путь на
машине клиента. Тот же чат, открытый с другой машины (CLAUDE.md и README:
владелец — отпечаток токена), получит несуществующий путь → явная
`ToolServerUnavailable` с причиной, а не молчаливый отказ.

**Окно `sliding_window` в сообщениях.** Ход с инструментами занимает
несколько сообщений, поэтому окно из N сообщений вмещает меньше
реплик пользователя. Поведение не меняется; при жалобах — отдельная
настройка счёта окна по ходам.

**Гонка продолжений.** Два одновременных запроса с `tool_results` к
одному чату пройдут проверку последнего сообщения до вызова модели и
запишут ход дважды. Смягчение — повторять проверку «последнее сообщение
ветки не изменилось» внутри транзакции записи (`BEGIN IMMEDIATE` в
`store::append_messages`) и отвечать `409`. В плане не детализировано:
TUI не шлёт параллельных продолжений в один чат, `ui.pending` блокирует
ввод.

**Рекурсивная проверка `api_key` в теле.** `contains_api_key` в
`app.rs` ищет ключ `api_key` на любой глубине тела. Схема инструмента
или аргументы вызова со свойством `api_key` получат `400`. У
`mcp-server-git` таких свойств нет; для будущих MCP-серверов — открытый
вопрос.

**`InvariantGuard` не проверяет аргументы вызовов.** Сознательный обмен
стоимости на покрытие: пишущие вызовы видит человек, читающие побочных
эффектов не имеют.

**Лок-файл сервиса уже сейчас путевой.** В закоммиченном
`agent-sever/Cargo.lock` (`git show HEAD:Cargo.lock`) у пакета
`agentcore` нет строки `source`: последний лок-файл был сгенерирован и
закоммичен с активным `[patch]`, вопреки порядку из CLAUDE.md. Сборка
образа и CI с `--locked` без `[patch]` на таком файле, вероятно,
падает или перегенерирует его. Задача 3.1 это исправляет; проверка в 3.4
не даёт закоммитить путевой лок-файл снова.

**Расхождения документации с кодом** (план опирается на код):

- `/Users/egor_lyadskiy/ai/CLAUDE.md`: «рабочие параметры … хранятся в
  файле самого чата в `~/.config/agentcli/chats/*.json`» — по коду
  (`crates/cli/src/chats.rs`, «Своего хранилища у клиента нет») и по
  README `agent-cli` («Где хранятся чаты») чаты и их настройки хранит
  `agentd`.
- CLAUDE.md: `tui.rs` «~2100 строк» — фактически 7331.
- CLAUDE.md: `agent-cli` описан как два крейта (`crates/core`,
  `crates/cli`) — в workspace четыре (`core`, `upstream`, `client`,
  `cli`); `agent/http.rs`, упомянутого в CLAUDE.md, нет — облачный
  провайдер живёт в `crates/upstream/src/lib.rs`.
- `agent-cli/README.md`: окно `Ctrl+P` — «список из шести разделов»; в
  `tui.rs` `SettingsSection::ALL` содержит семь. После задачи 4.5 их
  станет восемь; README исправляется в задаче 5.1.

**Сознательно отложено:** другие MCP-серверы и общий реестр серверов в
конфиге; удалённые транспорты MCP (streamable HTTP); ресурсы, промпты и
`list_changed`; `structuredContent` и нетекстовые результаты;
стриминг ответов модели; параллельное выполнение вызовов; режим «разрешить
все пишущие вызовы до конца хода»; фильтр секретов в теле запроса к
провайдеру.
