## Purpose

Определяет MCP-сервер `index-mcp serve` и его использование клиентом
`agentcli`: инструменты индекса документов, настройки, команды, раздел TUI
и инструменты модели в чатах.

## ADDED Requirements

### Requirement: Сервер отдаёт инструменты индекса
`index-mcp serve --db <файл>` SHALL запускать MCP-сервер на stdio с
инструментами `index_search`, `index_status`, `index_models`, `index_build`
и `index_compare`, а результат каждого SHALL быть JSON в
`structuredContent`. Подкоманды `build` и `compare`, их флаги и умолчания
SHALL остаться прежними.

#### Scenario: Список инструментов
- **WHEN** клиент запрашивает `tools/list` у `index-mcp serve`
- **THEN** сервер возвращает ровно эти пять инструментов

#### Scenario: Умолчания сборки
- **WHEN** `index_build` вызван без `min_chars`, `chunk_size` и `model`
- **THEN** действуют 50000 символов, 1200 и `nomic-embed-text`

### Requirement: Поиск не смешивает модели
`index_search` SHALL вернуть ошибку, а не результат, если модель запроса не
совпадает с моделью векторов выбранной стратегии.

#### Scenario: База построена другой моделью
- **WHEN** стратегия построена `mini-embed`, а сервер запущен с
  `--model nomic-embed-text`
- **THEN** `index_search` отвечает ошибкой «построена моделью mini-embed» и не
  обращается к Ollama

### Requirement: Размерность берётся у модели
`index_build` без `dim` SHALL определять размерность по `/api/show`, а если
её там нет — по первому вектору, и SHALL записывать её в `builds`.

#### Scenario: Модель не 768
- **WHEN** `index_build` вызван с моделью, чей `embedding_length` равен 32
- **THEN** результат содержит `dim: 32`, а векторы в базе имеют длину 32

### Requirement: Настройки индекса в конфиге клиента
Система SHALL хранить в `Config` поля `index_root`, `index_db`,
`index_strategy`, `index_model`, `index_unit`, `index_chunk_size`,
`index_overlap`, `index_max_section`, `index_min_section`,
`index_ollama_url` и SHALL править их командой `agentcli config index
set|show|clear` и разделом «Индекс документов» в `Ctrl+P`. Неизвестные
стратегия и единица SHALL отклоняться.

#### Scenario: Снятие значения
- **WHEN** выполнено `config index set --model ""`
- **THEN** `index_model` не задан и `show` показывает умолчание

### Requirement: Команды индекса
`agentcli index build|search|status|models` SHALL запускать `index-mcp` на
время команды, печатать ход сборки в stderr и итог в stdout.

#### Scenario: Поиск без базы в конфиге
- **WHEN** `index_db` не задан и выполнено `agentcli index search "вопрос"`
- **THEN** команда завершается ошибкой с подсказкой `config index set --db`

### Requirement: Инструменты индекса в чатах
При заданном `index_db` модель в чатах и `agentcli ask` SHALL получать
`index_search`, `index_status` и `index_build`. `index_build` SHALL быть
пишущим: в TUI требовать подтверждения, в `ask` отклоняться.

#### Scenario: Сборка в ask
- **WHEN** модель в `agentcli ask` вызывает `index_build`
- **THEN** вызов отклоняется без запуска сборки
