## Why

Консольный клиент `agentcli` сегодня сам обращается к облачному провайдеру:
берёт `api_key` и `base_url` из `~/.config/agentcli/config.toml` и шлёт
`POST {base_url}/chat/completions`. Ключ провайдера лежит на машине каждого
пользователя, а сервис `agentd`, которому этот ключ принадлежит, остаётся в
стороне от основного пути запроса.

Облачная модель должна быть доступна клиенту только через `agentd`
(`POST /v1/chat`), а запрет — держаться графом зависимостей, а не
договорённостью: у крейта `agentcli` не должно быть доступа к коду, который
умеет звать облачного провайдера напрямую. Локальный Ollama остаётся
исключением: запросы к нему не покидают машину пользователя и ключа не
требуют.

## What Changes

- Workspace `agent-cli` разбивается на четыре крейта:
  - `crates/core` (`agentcore`) — трейт `Agent`, типы сообщений и телеметрии,
    `config.rs`, `pipeline.rs`, `logging.rs`, `agent/error.rs` и локальный
    Ollama вместе с публичной реализацией `Agent` для него. Кода обращения к
    облачному провайдеру здесь больше нет;
  - `crates/upstream` (`agentupstream`, новый) — облачная часть нынешнего
    `HttpAgent`: тело Chat Completions, ключ, `base_url`, разбор `usage` и
    `reasoning_content`. Единственный потребитель — сервис `agentd`;
  - `crates/client` (`agentclient`, новый) — `ServerAgent`: реализация `Agent`
    поверх `POST {server_url}/v1/chat`;
  - `crates/cli` (`agentcli`) — зависит только от `agentcore` и `agentclient`.
- **BREAKING** `Provider::Cloud` в клиенте означает «через сервис `agentd`», а
  не «прямо в облачный API». Значение `Provider` в файлах чатов не меняется,
  старые чаты открываются без миграции.
- **BREAKING** Из конфига клиента убираются `api_key` и `base_url`; вместо них
  появляются `server_url` (по умолчанию `http://127.0.0.1:8080`) и
  `client_token`. Команда `config set-key` заменяется на `config set-token`,
  `config set-url` задаёт адрес сервиса, `config show` печатает `server_url` и
  маскированный токен, `config models` берёт список из
  `GET {server_url}/v1/models`. Старый конфиг читается без ошибки, но клиент
  один раз печатает предупреждение и файл молча не переписывает.
- В TUI раздел «Подключение» (`Ctrl+P`) для облачного чата содержит адрес
  сервиса, токен и модель вместо Base URL и ключа; раздел Ollama не меняется.
- Ошибки сервиса (`401`, `400`, `422`, `429`, `502`, `504`, транспорт)
  превращаются в типизированный `AgentError` с показом `request_id`.
- Репозиторий `agent-sever` переходит с `agentcore::agent::HttpAgent` на
  `agentupstream`; контракт `/v1` и поведение сервиса не меняются.

## Capabilities

### New Capabilities
- `client-transport`: путь запроса консольного клиента — облачный чат идёт
  только через `agentd`, локальный Ollama идёт напрямую, а прямой вызов
  облачного провайдера недостижим из графа зависимостей `agentcli`.
- `client-config`: конфигурация клиента и её команды — адрес сервиса,
  клиентский токен, список моделей от сервиса, чтение устаревшего конфига.

### Modified Capabilities
<!-- Каталог openspec/specs пуст: опубликованных спецификаций, чьи требования
     менялись бы, в проекте пока нет. -->

## Impact

- `agent-cli`: корневой `Cargo.toml` (члены workspace), новые крейты
  `crates/upstream` и `crates/client`, удаление `crates/core/src/agent/http.rs`
  в пользу `agentupstream`, правки `crates/core/src/config.rs`,
  `crates/cli/{main.rs,cli.rs,tui.rs}`, README.
- `agent-sever`: `Cargo.toml` (вторая git-зависимость на тот же репозиторий),
  локальный `.cargo/config.toml` с `[patch]` на два крейта, замена типа агента
  в `state.rs`/`app.rs`, README.
- Новых внешних зависимостей не добавляется, кроме `wiremock` в dev-зависимости
  крейта `agentclient`.
- Пользователи после обновления обязаны запустить `agentd` и задать
  `server_url`/`client_token`: без сервиса облачные чаты не работают.
