## Why

Изменение `configure-max-context-tokens` добавило клиентский лимит контекста
как одно значение в глобальном `Config`, общее для всех чатов — решение,
принятое из-за того, что `agent-sever` на тот момент не хранил
`max_context_tokens` нигде, кроме одного вызова `POST /v1/chat`
(`agent-sever/openspec/changes/limit-context-window`). Пользователю такое
поведение не подходит: разные чаты работают с разными моделями и разным
реальным окном контекста, и общий лимit вынуждает либо использовать самый
тесный лимит для всех чатов сразу, либо постоянно его менять при переключении
между ними. Правильное место для значения — настройки конкретного чата,
как уже устроены `model`, `reasoning`, `thinking`, `experts`. Это требует
согласованного изменения на стороне `agent-sever`
(`persist-chat-context-limit`): без постоянного хранилища лимита в настройках
чата клиент не может считать значение «настройкой чата» — оно всё равно
терялось бы при следующем открытии.

## What Changes

- **BREAKING** (относительно ещё не заархивированного `configure-max-context-tokens`):
  `max_context_tokens` больше не хранится как отдельное поле `ServerAgent`,
  подставляемое из `Config` в каждый запрос. Вместо этого
  `agentcore::ChatSettings` получает поле `max_context_tokens: Option<u32>` —
  лимит контекста конкретного чата, как `model`.
- `Config.max_context_tokens` остаётся, но меняет назначение: это лимит по
  умолчанию для НОВЫХ чатов (как `Config.model`, `Config.reasoning`), а не
  значение, отправляемое в каждый запрос напрямую. `Config::default_chat_settings`
  копирует его в `ChatSettings.max_context_tokens` нового чата.
- `ServerAgent::new` перестаёт принимать `max_context_tokens` параметром;
  `build_body` берёт значение из `settings.max_context_tokens` (параметр
  запроса), как уже делает для `model` через `model_for`.
- `agentcli config context-limit set/clear/show` меняет семантику: теперь
  управляет умолчанием для новых чатов (`Config.max_context_tokens`), а не
  значением, действующим здесь и сейчас для всех чатов сразу — по аналогии с
  `config set-model`.
- В TUI поле «Лимит контекста» переезжает из раздела «Подключение»
  (`server_url`/`client_token`/`ollama_url` — общие для клиента) в состав
  полей чата, сохраняемых через `PATCH /v1/chats/{id}`
  (`request_update_chat`), рядом с моделью — как поле **Модель**, которое уже
  живёт в том же разделе UI, но относится к конкретному чату.
- Требует согласованного изменения `agent-sever`
  (`persist-chat-context-limit`): `ChatSettingsDto.max_context_tokens`
  должен не только сужать лимит на один вызов, но и сохраняться в
  постоянных настройках чата при `PATCH /v1/chats/{id}`, применяясь затем при
  каждом `POST /v1/chat` для этого чата, если вызов явно не передал своё
  значение.

## Capabilities

### New Capabilities

Нет: это не новая возможность, а пересмотр решения из ещё не
заархивированного `client-context-limit`.

### Modified Capabilities

- `client-context-limit` (введена `configure-max-context-tokens`, изменение
  ещё не заархивировано — прежняя спека правится напрямую как черновик,
  а не через delta): лимит контекста становится настройкой конкретного чата
  (`ChatSettings.max_context_tokens`), а не общим значением клиента
  (`Config.max_context_tokens` остаётся лишь умолчанием для новых чатов).

## Impact

- Код: `crates/core/src/config.rs` (`ChatSettings.max_context_tokens`,
  `Config::default_chat_settings`), `crates/client/src/lib.rs`
  (`ServerAgent::new` — убрать параметр, `build_body` — брать значение из
  `settings`), `crates/cli/src/agent.rs` (убрать проброс в `ServerAgent::new`),
  `crates/cli/src/main.rs` (`run_context_limit_action` — семантика
  умолчания для новых чатов), `crates/cli/src/tui.rs` (поле переезжает в
  состав `ChatSettings`, сохраняется через `request_update_chat`, а не
  `save_connection`).
- Другой репозиторий: `agent-sever` — параллельное изменение
  `persist-chat-context-limit` (хранение `max_context_tokens` в настройках
  чата, миграция БД, `merge_settings`, `effective_context_limit`). Текущий
  клиентский код неизбежно шлёт значение поверх нового контракта раньше, чем
  сервис научится его сохранять, — это допустимо: сервис и сегодня принимает
  и применяет `settings.max_context_tokens` на один вызов, просто не хранит
  его; после `persist-chat-context-limit` то же поле в `PATCH /v1/chats/{id}`
  начнёт сохраняться.
- Вне области: валидация значения относительно операторского лимита — как и
  прежде, целиком на стороне `agent-sever`.
