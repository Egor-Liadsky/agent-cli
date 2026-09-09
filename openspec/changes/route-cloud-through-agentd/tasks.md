## 1. Разделение крейтов без изменения поведения

- [x] 1.1 Создать крейт `crates/upstream` (`agentupstream`, `lib`), добавить его
  и `crates/client` в члены workspace и в `[workspace.dependencies]`; проверка:
  `cargo metadata --no-deps` показывает четыре пакета.
- [x] 1.2 Перенести облачную часть `crates/core/src/agent/http.rs` (сборка тела
  Chat Completions, ключ, `base_url`, разбор `usage` и `reasoning_content`,
  типизация ошибок провайдера) в `agentupstream` вместе с её тестами;
  проверка: `cargo test -p agentupstream` зелёный.
- [x] 1.3 Выделить в `agentcore` публичную реализацию `Agent` для локального
  Ollama и оставить в ядре общую часть (системный промпт, выбор модели,
  `AgentError`, `ExchangeLog`); удалить `agent/http.rs`. Проверка:
  `cargo test -p agentcore` зелёный, `grep -r "chat/completions" crates/core`
  ничего не находит.
- [x] 1.4 Временно подключить `agentupstream` к `agentcli` и восстановить
  нынешнее поведение клиента (диспетчеризация по `Provider` на стороне
  клиента). Проверка: `cargo build`, `cargo test`, ручной прогон
  `cargo run -p agentcli -- ask "привет"` и `chat` работают как прежде.
- [x] 1.5 Создать крейт `crates/client` (`agentclient`, `lib`) с зависимостью
  на `agentcore` и dev-зависимостью `wiremock`; проверка: `cargo build -p
  agentclient` проходит.

## 2. `ServerAgent` и его тесты

- [x] 2.1 Расширить `AgentError` вариантами для отказа политики, неизвестного
  клиентского токена и невалидного запроса и добавить `request_id`; обновить
  отображение ошибок на стороне сервиса, если требуется. Проверка:
  `cargo test -p agentcore` и `cargo test` в `agent-sever` зелёные.
- [x] 2.2 Реализовать `ServerAgent` в `agentclient`: `POST {server_url}/v1/chat`
  с историей в `messages` и параметрами чата в `settings`, заголовок
  `Authorization: Bearer` при непустом токене, разбор `content`, `reasoning`,
  `model`, `usage`, `timing`, `policy` в `AgentReply`.
- [x] 2.3 Тесты `ServerAgent` против `wiremock`: успешный ответ с разбором
  `usage`/`timing`, коды `401`, `400`, `422`, `429`, `502`, `504` дают
  соответствующие варианты `AgentError` с `request_id`, недоступный адрес даёт
  ошибку транспорта без паники. Проверка: `cargo test -p agentclient` зелёный.
- [x] 2.4 Тесты тела запроса: поле `api_key` отсутствует, история лежит в
  `messages`, заголовок `Authorization` отсутствует при пустом токене и
  присутствует при заданном. Проверка: те же тесты зелёные.
- [x] 2.5 Реализовать в `agentclient` запрос списка моделей
  `GET {server_url}/v1/models` с типизированной ошибкой при недоступном
  сервисе; проверка: тест против `wiremock` на успешный список и на отказ.

## 3. Переключение клиента на сервис

- [x] 3.1 Заменить в `agentcli` облачного агента на `ServerAgent`, оставив
  прямой путь в Ollama; убрать зависимость на `agentupstream`. Проверка:
  `cargo tree -p agentcli` не содержит `agentupstream`, `cargo build` и
  `cargo test` зелёные.
- [x] 3.2 Провести ручной прогон против запущенного `agentd`: облачный чат
  отвечает, телеметрия и модель из ответа показываются, чат Ollama работает
  без сервиса.
- [x] 3.3 Проверить, что запрос облачного чата уходит только на `server_url`:
  журнал обмена клиента (`requests.jsonl`) не содержит адреса провайдера.

## 4. Конфигурация клиента

- [x] 4.1 Заменить в конфиге клиента `api_key`/`base_url` на `server_url`
  (по умолчанию `http://127.0.0.1:8080`) и `client_token` с маскированием;
  проверка: юнит-тест на значения по умолчанию и на маскирование.
- [x] 4.2 Обеспечить чтение устаревшего конфига с `api_key`/`base_url` без
  ошибки и однократное предупреждение без перезаписи файла; проверка:
  юнит-тест разбирает старый конфиг и не падает.
- [x] 4.3 Заменить команду `config set-key` на `config set-token`, перевести
  `config set-url` на адрес сервиса, обновить `config show`; проверка:
  `cargo run -p agentcli -- config show` печатает `server_url` и маскированный
  токен и не упоминает ключ провайдера.
- [x] 4.4 Перевести `config models` на `GET {server_url}/v1/models` с понятной
  ошибкой при недоступном сервисе; проверка: команда печатает список при
  работающем `agentd` и осмысленную ошибку при выключенном.
- [ ] 4.5 Обновить раздел «Подключение» (`Ctrl+P`) в TUI: адрес сервиса, токен
  и модель для облачного чата, раздел Ollama без изменений, сохранение по
  `Ctrl+S` применяется к следующим запросам без перезапуска. Проверка: ручной
  прогон TUI.

## 5. Перевод сервиса на `agentupstream`

- [ ] 5.1 Запушить `agent-cli` в `main` (разделение крейтов и правки ядра).
- [x] 5.2 В `agent-sever` добавить git-зависимость `agentupstream` на тот же
  репозиторий и обновить локальный `.cargo/config.toml`: `[patch]` подменяет
  оба крейта путями `../agent-cli/crates/core` и `../agent-cli/crates/upstream`.
  Выполнено вместе с задачей 5.3.
- [x] 5.3 Заменить в сервисе `agentcore::agent::HttpAgent` на агента из
  `agentupstream`, сохранив контракт `/v1` и выбор между облаком и Ollama;
  проверка: `cargo test` в `agent-sever` зелёный. Выполнено досрочно на шаге 2:
  расширение `AgentError` ломало сборку сервиса, а он собирается против
  локального `[patch]`.
- [ ] 5.4 Проверить сборку против запушенного `main`: временно убрать
  `[patch]`, выполнить `cargo generate-lockfile` и `cargo test --locked`;
  убедиться, что `Cargo.lock` содержит коммиты обоих крейтов
  (`grep -A2 'name = "agentcore"' Cargo.lock`, то же для `agentupstream`).

## 6. Документация

- [ ] 6.1 Обновить README `agent-cli`: новая схема запуска (сначала `agentd`,
  потом клиент), таблица команд `config`, поля конфига `server_url` и
  `client_token`, раздел про Ollama как исключение, описание крейтов
  workspace.
- [ ] 6.2 Обновить README `agent-sever`: сервис как единственный владелец ключа
  провайдера, зависимость на `agentupstream`, локальный `[patch]` на два
  крейта.
- [ ] 6.3 Итоговая проверка: `cargo build` и `cargo test` зелёные в обоих
  репозиториях, `openspec validate route-cloud-through-agentd --strict`
  проходит.
