## Why

Сервис `agentd` (изменение `limit-context-window` в `agent-sever`) принимает
`settings.max_context_tokens` в `POST /v1/chat`: клиент может этим полем
только сузить операторский лимит контекстного окна на один запрос, а
превышение — большого лимита либо истории — сервис отклоняет `400`
(`context_limit_invalid` или `context_limit_exceeded`). Значение — не часть
персистентных настроек чата: сервис не пишет его в хранилище ни при разовом
вызове, ни через `PATCH /v1/chats/{id}` (`agentcore::ChatSettings`, общий
тип ядра и сервиса, этого поля не содержит и содержать не должен — оно
осталось транспортным полем контракта `agent-sever`). Сейчас `agentcli`
это поле не знает и не отправляет: клиент не может настроить более узкий
лимит контекста, а отказ сервиса при превышении истории приходит клиенту
как обычная ошибка API без узнаваемого сообщения.

## What Changes

- Глобальный конфиг клиента (`Config`, `crates/core/src/config.rs`)
  пополняется необязательным полем `max_context_tokens: Option<u32>` — по
  аналогии с `model`: один клиентский умолчательный лимит, применяемый ко
  всем чатам и запросам, а не настройка отдельного чата (у `ChatSettings`
  для него нет постоянного хранилища — см. Why).
- `ServerAgent` (`crates/client/src/lib.rs`) принимает это значение в
  `ServerAgent::new` (как уже принимает `model`) и передаёт его в поле
  `settings.max_context_tokens` тела `POST /v1/chat`, когда оно задано,
  для обоих путей (`ask`, `ask_in_chat`).
- `agentcli config` получает новую подкоманду для чтения и изменения этого
  значения — `config context-limit set/clear/show`, по образцу
  `config sampling`/`config format`.
- В TUI (`crates/cli/src/tui.rs`) добавляется поле «Лимит контекста
  (токены, общий для всех чатов)» в разделе подключения панели настроек
  (`SettingsSection::Connection`), рядом с адресом сервиса и токеном —
  сохраняется в глобальный конфиг тем же путём, что и они, а не в
  `ChatSettings` конкретного чата.
- Ответы сервиса с кодами `context_limit_invalid` и `context_limit_exceeded`
  показываются клиенту как есть — новая обработка кодов в `AgentError` не
  нужна: обе ошибки приходят `400` и уже попадают в
  `AgentError::InvalidRequest` (`crates/client/src/lib.rs`,
  `parse_service_error`).
- README `agent-cli` документирует новую настройку и команду `config
  context-limit`.

## Capabilities

### New Capabilities

- `client-context-limit`: необязательный клиентский лимит контекста в
  `Config`, его передача в `POST /v1/chat` и управление командой `config
  context-limit` и полем в TUI.

### Modified Capabilities

Нет: контракт `/v1/chat` уже описан со стороны сервиса
(`agent-sever/openspec/changes/limit-context-window`); существующая спека
`client-config` (`agent-cli`) описывает адрес сервиса и токен, но не общий
список полей `Config` — новая настройка не переопределяет её требования.

## Impact

- Код: `crates/core/src/config.rs` (`Config.max_context_tokens`),
  `crates/client/src/lib.rs` (`ServerAgent::new`, `ChatSettingsPayload`,
  `build_body`), `crates/cli/src/agent.rs` (`CliAgent::from_config`
  прокидывает значение в `ServerAgent::new`), `crates/cli/src/cli.rs`
  (`ConfigAction::ContextLimit`), `crates/cli/src/main.rs` (обработчик
  подкоманды), `crates/cli/src/tui.rs` (поле в разделе подключения панели
  настроек).
- Хранение: значение сериализуется в `~/.config/agentcli/config.toml`
  вместе с прочими полями `Config` — обратной совместимости для старого
  конфига без поля не требуется, `#[serde(default)]` даёт `None`.
- README `agent-cli`: раздел конфигурации и команд `config`.
- Вне области: операторский лимит и оценка размера истории — целиком на
  стороне `agent-sever` (уже реализовано); настройка лимита конкретного
  чата (а не общая для клиента) потребовала бы изменения контракта
  `agent-sever` — не предмет этого изменения.
</content>
