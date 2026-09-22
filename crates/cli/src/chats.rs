//! Чат в памяти клиента и его представление в TUI.
//!
//! Своего хранилища у клиента нет: список, история и настройки чата живут в
//! сервисе `agentd` (specs/client-chat-storage). Здесь остаётся только
//! модель чата на время работы клиента и форматирование для интерфейса.

use agentclient::{ChatHistory, ChatSummary};
use agentcore::agent::{Message, Role};
use agentcore::config::ChatSettings;

#[derive(Clone)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub messages: Vec<Message>,
    pub updated_at: u64,
    /// Параметры агента этого чата: у каждого чата они свои.
    pub settings: ChatSettings,
    /// История получена от сервиса. Пока нет, отправлять реплику нельзя:
    /// модель получила бы неполный контекст.
    pub history_loaded: bool,
}

impl ChatSession {
    /// Чат из элемента списка сервиса: без истории, она догружается при
    /// первом открытии чата.
    pub fn from_summary(summary: ChatSummary) -> Self {
        Self {
            id: summary.id,
            title: summary.title,
            messages: Vec::new(),
            updated_at: summary.updated_at.max(0) as u64,
            settings: summary.settings,
            history_loaded: summary.message_count == 0,
        }
    }

    /// Наложить историю, полученную от сервиса, на чат.
    pub fn apply_history(&mut self, history: ChatHistory) {
        self.title = history.chat.title;
        self.settings = history.chat.settings;
        self.updated_at = history.chat.updated_at.max(0) as u64;
        self.messages = history
            .messages
            .into_iter()
            .map(|stored| stored.message)
            .collect();
        self.history_loaded = true;
    }

    /// Обновить время изменения. Заголовок при этом не меняется: его
    /// придумывает сервис после первого обмена (`AGENTD_AUTO_TITLE`).
    pub fn touch_quietly(&mut self) {
        self.updated_at = agentcore::agent::now_secs().max(0) as u64;
    }
}

/// Текст, которым история другого чата переносится в текущий одним блоком.
pub fn context_block(chat: &ChatSession) -> String {
    let mut block = format!("[Контекст из чата «{}»]\n", chat.title);
    for message in &chat.messages {
        let label = match message.role {
            Role::User => "Вы",
            Role::Assistant => "Агент",
            Role::System => "Система",
            Role::Tool => "Инструмент",
        };
        block.push_str(&format!("{label}: {}\n", message.content));
    }
    block
}

/// Короткая метка времени последнего сообщения: время для сегодняшних чатов,
/// дата — для более старых.
pub fn last_activity_label(updated_at: u64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let Some(moment) = Local.timestamp_opt(updated_at as i64, 0).single() else {
        return String::new();
    };
    let now = Local::now();
    if moment.date_naive() == now.date_naive() {
        moment.format("%H:%M").to_string()
    } else if moment.year() == now.year() {
        moment.format("%d.%m %H:%M").to_string()
    } else {
        moment.format("%d.%m.%y").to_string()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use agentclient::{ChatSummary, StoredMessage};
    use agentcore::config::ChatSettings;

    fn summary(title: &str, message_count: i64) -> ChatSummary {
        ChatSummary {
            id: "chat-1".to_string(),
            title: title.to_string(),
            settings: ChatSettings::default(),
            created_at: 1000,
            updated_at: 2000,
            message_count,
        }
    }

    // --- 4.4 История накладывается на чат ---

    #[test]
    fn history_replaces_messages_and_marks_chat_loaded() {
        let mut chat = ChatSession::from_summary(summary("Новый чат", 2));
        assert!(!chat.history_loaded, "чат с сообщениями не загружен по списку");

        chat.apply_history(ChatHistory {
            chat: summary("Заголовок сервиса", 2),
            branch_id: None,
            messages: vec![
                StoredMessage {
                    seq: 1,
                    created_at: 1001,
                    message: Message::user("вопрос"),
                },
                StoredMessage {
                    seq: 2,
                    created_at: 1002,
                    message: Message::assistant("ответ"),
                },
            ],
        });

        assert!(chat.history_loaded);
        assert_eq!(chat.title, "Заголовок сервиса");
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[1].content, "ответ");
    }

    // --- 4.9 Блок контекста строится по загруженной истории ---

    #[test]
    fn context_block_lists_messages_with_speaker_labels() {
        let mut chat = ChatSession::from_summary(summary("Источник", 0));
        chat.messages.push(Message::user("вопрос"));
        chat.messages.push(Message::assistant("ответ"));

        let block = context_block(&chat);
        assert!(block.starts_with("[Контекст из чата «Источник»]"));
        assert!(block.contains("Вы: вопрос"));
        assert!(block.contains("Агент: ответ"));
    }

}
