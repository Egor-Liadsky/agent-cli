//! Данные вызова инструментов (tool calling): описание инструмента для
//! модели, вызов, который модель вернула, и выравнивание истории.
//!
//! Модуль намеренно не знает, кто и как выполняет инструменты: цикл
//! «вызов → результат» живёт у клиента, которому доступны процессы и
//! пользователь. Ядро лишь переносит вызовы между провайдерами и сервисом.

use super::{Message, Role};
use serde::{Deserialize, Serialize};

/// Описание инструмента, которое уходит модели.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema аргументов (для MCP — `inputSchema`).
    pub parameters: serde_json::Value,
}

/// Вызов инструмента, который вернула модель.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Идентификатор вызова. У OpenAI-совместимых провайдеров приходит от
    /// провайдера; у Ollama его нет — назначает ядро (`call_<n>`).
    pub id: String,
    pub name: String,
    /// Аргументы как JSON-объект. OpenAI-формат передаёт их строкой —
    /// перевод в объект и обратно делает `agentupstream`.
    #[serde(default = "empty_object")]
    pub arguments: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Текст синтетического результата для вызова, ответ на который так и не
/// пришёл (клиент упал посреди хода, ветка создана от середины хода).
pub const DANGLING_TOOL_RESULT: &str =
    "вызов инструмента не был выполнен: ход прерван до получения результата";

/// Закрывает «висячие» вызовы: для каждого `tool_calls` без ответа роли
/// `tool` добавляет синтетический результат, а ответы `tool` без вызова
/// выбрасывает.
///
/// Провайдеры отвергают историю, где за ответом модели с вызовами не идут
/// результаты всех этих вызовов, и историю с результатом, который ни на что
/// не отвечает. Такая история возникает законно — прерванный ход, ветка от
/// середины хода, — поэтому её выравнивают перед каждым запросом, а не
/// считают ошибкой.
///
/// Результат относится к вызову, если стоит сразу после ответа модели (в
/// сплошной группе сообщений `tool`) и совпадает с ним по `tool_call_id`, а
/// при отсутствии идентификатора — по имени инструмента.
pub fn close_dangling_tool_calls(history: &[Message]) -> Vec<Message> {
    let mut out = Vec::with_capacity(history.len());
    let mut i = 0;
    while i < history.len() {
        let message = &history[i];
        i += 1;
        if matches!(message.role, Role::Tool) {
            // Результат вне группы после вызова отвечать не на что.
            continue;
        }
        out.push(message.clone());
        if message.tool_calls.is_empty() {
            continue;
        }
        let mut pending: Vec<&super::ToolCall> = message.tool_calls.iter().collect();
        while i < history.len() && matches!(history[i].role, Role::Tool) {
            let result = &history[i];
            i += 1;
            let position = pending.iter().position(|call| match &result.tool_call_id {
                Some(id) => &call.id == id,
                None => result.tool_name.as_deref() == Some(call.name.as_str()),
            });
            if let Some(position) = position {
                let call = pending.remove(position);
                let mut result = result.clone();
                // Недостающие связи восстанавливаются из вызова: провайдеру
                // OpenAI нужен `tool_call_id`, Ollama — `tool_name`.
                result.tool_call_id.get_or_insert_with(|| call.id.clone());
                result.tool_name.get_or_insert_with(|| call.name.clone());
                out.push(result);
            }
        }
        for call in pending {
            out.push(Message::tool_result(
                call.id.clone(),
                call.name.clone(),
                DANGLING_TOOL_RESULT,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    fn shape(history: &[Message]) -> Vec<(String, Option<String>)> {
        history
            .iter()
            .map(|m| {
                let role = serde_json::to_value(m.role).unwrap();
                (role.as_str().unwrap().to_string(), m.tool_call_id.clone())
            })
            .collect()
    }

    #[test]
    fn clean_history_is_unchanged() {
        let history = vec![
            Message::user("статус?"),
            Message::assistant_with_tool_calls("", vec![call("call_0", "git_status")]),
            Message::tool_result("call_0", "git_status", "clean"),
            Message::assistant("всё чисто"),
        ];
        let closed = close_dangling_tool_calls(&history);
        assert_eq!(shape(&closed), shape(&history));
        assert_eq!(closed[2].content, "clean");
    }

    #[test]
    fn dangling_call_gets_synthetic_result() {
        let history = vec![
            Message::user("статус и лог"),
            Message::assistant_with_tool_calls(
                "",
                vec![call("call_0", "git_status"), call("call_1", "git_log")],
            ),
            Message::tool_result("call_0", "git_status", "clean"),
        ];
        let closed = close_dangling_tool_calls(&history);
        assert_eq!(closed.len(), 4);
        assert_eq!(closed[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(closed[3].tool_name.as_deref(), Some("git_log"));
        assert_eq!(closed[3].content, DANGLING_TOOL_RESULT);
    }

    #[test]
    fn orphan_result_is_dropped() {
        let history = vec![
            Message::user("привет"),
            Message::tool_result("call_9", "git_status", "лишний"),
            Message::assistant("здравствуйте"),
            Message::assistant_with_tool_calls("", vec![call("call_0", "git_status")]),
            Message::tool_result("call_0", "git_status", "clean"),
            Message::tool_result("call_7", "git_log", "чужой"),
        ];
        let closed = close_dangling_tool_calls(&history);
        let contents: Vec<&str> = closed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["привет", "здравствуйте", "", "clean"]);
    }

    #[test]
    fn result_without_id_matches_by_name() {
        let mut result = Message::tool_result("x", "git_status", "clean");
        result.tool_call_id = None;
        let history = vec![
            Message::assistant_with_tool_calls("", vec![call("call_0", "git_status")]),
            result,
        ];
        let closed = close_dangling_tool_calls(&history);
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[1].tool_call_id.as_deref(), Some("call_0"));
    }
}
