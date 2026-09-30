## Context

`index-mcp` хранит чанки и векторы в SQLite (`chunks`, `embeddings`,
`builds`) и умеет `build`/`compare`. `pipeline-mcp` и `git-mcp` — образцы
серверов, которые клиент запускает процессом на команду или ход; их
контракт — имена инструментов и JSON в `structuredContent`.

## Goals / Non-Goals

**Goals:** MCP-режим `index-mcp` без дублирования логики chunking,
эмбеддинга и хранилища; клиентская часть по образцу `pipeline.rs`;
настройки в конфиге, командах и `Ctrl+P`; инструменты в чатах.

**Non-Goals:** ANN-индекс, гибридный поиск, инкрементальная пересборка,
показ `index_compare` модели чата, шаги сборки как MCP-уведомления о
прогрессе.

## Decisions

**Контракт инструментов** (общий для сервера и клиента; переименование
требует правки `crates/cli/src/index.rs`):

| Инструмент | Пишет | Аргументы | Результат |
|------------|-------|-----------|-----------|
| `index_search` | нет | `query`; `strategy`; `top_k` (5, максимум 20) | `{query, strategy, model, dim, hits:[{chunk_id, source, section, score, text}]}` |
| `index_status` | нет | — | `{db, exists, search_model, strategies:[{strategy, chunks, files, chars, model, dim, embed_ms, built_at, params}]}` |
| `index_models` | нет | — | `{models:[{name, dim, context_length, size}]}` |
| `index_build` | базу | `input`; `strategy`, `unit`, `chunk_size`, `overlap`, `max_section`, `min_section`, `min_chars`, `model`, `num_ctx`, `batch`, `dim` | `{db, model, dim, strategies:[{strategy, chunks, files, chars, embed_ms}]}` |
| `index_compare` | отчёт | `questions`, `out` | `{out, report}` |

Ошибка инструмента — `isError` с русским текстом. Сервер запускается
`index-mcp serve --db <файл> [--strategy] [--ollama-url] [--model]
[--doc-prefix] [--query-prefix] [--batch] [--num-ctx]`.

**`serve` поверх тех же функций.** Умолчания `build` вынесены в модуль
`defaults` (их берёт и clap, и инструмент); `index_build` собирает те же
`BuildArgs`/`EmbedArgs` и зовёт `run_build`, `index_compare` — `run_compare`.
Флаги CLI не менялись, единственная правка `run_build` — строки хода в
stderr (у `serve` stdout занят протоколом, у клиента `index build` stderr
показывается в терминале, а в TUI идёт в строку раздела).

**Почему `index_build` пишущий.** Он заменяет строки стратегии в базе и
минуты грузит Ollama; модель не должна запускать это без ведома человека.
Поэтому `IndexTools::is_write` истинно для всего, кроме
`READ_ONLY_TOOLS` (`index_search`, `index_status`, `index_models`): в TUI
вызов идёт через `ToolApprover`, в `ask` `DenyWrites` его отклоняет. Каталог
и параметры сборки подставляются из настроек, что модель не назвала сама, и
`input` убран из `required` схемы, когда каталог задан, — модель не выдумывает
путь. `index_models` и `index_compare` человеку нужны, разговору — нет, поэтому
`CHAT_TOOLS = [index_search, index_status, index_build]`.

**Совпадение модели.** Вопрос эмбеддится моделью, с которой запущен сервер
(`--model`, клиент передаёт `index_model`). `index_search` до обращения к
Ollama сверяет её с `builds.model` выбранной стратегии, затем с
`embeddings.model` и длиной вектора каждого чанка; расхождение — ошибка
«построена моделью X … векторы несравнимы», результат не возвращается.
Стратегия, если не названа и в базе их несколько, тоже ошибка, а не выбор
наугад.

**Откуда размерность.** Умолчание `--dim 768` верно только для
`nomic-embed-text`. `index_build` без `dim` берёт `<arch>.embedding_length`
из `/api/show`; если Ollama её не сообщил — длину первого вектора
(`Embedder::probe_dim`). Эмбеддер по-прежнему сверяет каждый вектор с этой
размерностью. У флага CLI `--dim` умолчание прежнее.

**`index_compare` перенесён.** Он укладывается в `run_compare` без
переписывания. `out` без умолчания: иначе вызов без аргумента перезаписал бы
`docs/chunking-comparison.md`. Клиент его модели не отдаёт.

**Процесс на команду или ход.** Как у `pipeline-mcp`: состояния между вызовами
нет, база — файл. `Progress` выбирает, куда девать stderr сервера: терминал
(команды), строка раздела (кнопка в TUI), `/dev/null` (ходы чата, где вывод
рисовал бы поверх экрана).

**Настройки — поля `Config`, а не `ChatSettings`.** Индекс один на машину,
как демон сводок и каталоги пайплайна. Включает инструменты заданный
`index_db` (отдельного переключателя нет). Единственная зависимость ядра —
сами поля; `clap`, `ratatui`, `rmcp` в ядро не попадают.

## Risks / Trade-offs

- Список моделей с capability `embedding` требует Ollama с полем
  `capabilities` в `/api/show`; старый Ollama покажет пустой список, поле
  модели остаётся текстовым и принимает ввод вручную.
- `index_build` в ходе чата блокирует ход на минуты (тайм-аут 30 минут);
  прогресса в чате нет.
- Модель, выбранная в `index_build` вызовом, отличается от `index_model` — поиск
  затем откажет; это осознанно, а не молчаливая смесь.
