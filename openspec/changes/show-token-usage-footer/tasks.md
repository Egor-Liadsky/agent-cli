## 1. Строка телеметрии в панели ввода

- [x] 1.1 В `render_pane` (`tui.rs:2123`) выделить условную дополнительную
      строку над рамкой ввода: 0 строк, если для текущего чата нет сообщения
      ассистента с телеметрией и нет накопленной суммы по чату, иначе 1
      строка (`token_footer_line`); `cargo build -p agentcli` — чисто.
- [x] 1.2 Найти последнее сообщение ассистента в
      `state.chats[chat_index].messages` и получить его `meta_token_summary`
      (`tui.rs:2337`, вызывается из `token_footer_line`); выводится как
      «Обмен: …».
- [x] 1.3 Вызвать существующий `chat_token_totals(&messages)` (`tui.rs:2410`)
      и отформатировать через `token_counters` как «Чат: …» в той же строке
      (`token_footer_line`).
- [x] 1.4 Стилизовать строку (`Color::DarkGray`, без рамки, `render_token_footer`)
      так, чтобы она не сливалась с текстом ввода; логика показа/скрытия
      проверена по коду и `cargo build`. Живая проверка в
      `cargo run -p agentcli -- chat` не выполнена в этой сессии — нет
      интерактивного терминала и поднятого `agentd`; нужна ручная проверка
      пользователем.

## 2. Очистка и тесты

- [x] 2.1 Убедиться, что `meta_token_summary` не вызывается больше нигде
      кроме новой строки статуса — `grep -n meta_token_summary tui.rs`
      показывает ровно определение (`tui.rs:2385`) и один вызов
      (`tui.rs:2167`, из `token_footer_line`).
- [x] 2.2 Добавлены в `mod tests` (`tui.rs`) тесты `chat_token_totals_sums_partial_fields_across_messages`
      и `chat_token_totals_of_empty_history_has_no_fields`: частично разные
      поля `MessageMeta` суммируются корректно, пустая история без паники
      даёт все `None`; `cargo test -p agentcli` зелёный (30 passed).
- [x] 2.3 Добавлены тесты `token_counters_is_none_for_empty_totals`,
      `token_counters_orders_parts_prompt_completion_reasoning_total`,
      `token_footer_line_is_none_without_any_telemetry`,
      `token_footer_line_combines_exchange_and_chat_totals` — фиксированный
      порядок частей (запрос → ответ → рассужд. → всего) и пустой случай.
- [x] 2.4 `cargo test` и `cargo build --release` в `agent-cli` прогнаны —
      без предупреждений о dead code для
      `TokenTotals`/`token_counters`/`chat_token_totals`/`meta_token_summary`/
      `token_footer_line`.
