use crate::agent::{Message, Role};
use crate::config::ChatSettings;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_TITLE: &str = "Новый чат";

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub messages: Vec<Message>,
    pub updated_at: u64,
    /// Параметры агента этого чата: у каждого чата они свои.
    #[serde(default)]
    pub settings: ChatSettings,
}

impl ChatSession {
    pub fn new(settings: ChatSettings) -> Self {
        Self {
            id: generate_id(),
            title: DEFAULT_TITLE.to_string(),
            messages: Vec::new(),
            updated_at: now(),
            settings,
        }
    }

    /// Обновить время изменения, не трогая заголовок: импортированный контекст
    /// не должен становиться названием чата.
    pub fn touch_quietly(&mut self) {
        self.updated_at = now();
    }

    pub fn touch(&mut self) {
        self.touch_quietly();
        if self.title == DEFAULT_TITLE {
            if let Some(first_user) = self.messages.iter().find(|m| matches!(m.role, Role::User)) {
                let mut title: String = first_user.content.chars().take(40).collect();
                if first_user.content.chars().count() > 40 {
                    title.push('…');
                }
                self.title = title;
            }
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn generate_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn chats_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("не удалось определить домашнюю директорию конфигов")?;
    Ok(dir.join("agentcli").join("chats"))
}

fn chat_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub fn save_chat(chat: &ChatSession) -> Result<()> {
    let dir = chats_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("не удалось создать директорию {}", dir.display()))?;
    let path = chat_path(&dir, &chat.id);
    let content = serde_json::to_string_pretty(chat).context("не удалось сериализовать чат")?;
    std::fs::write(&path, content)
        .with_context(|| format!("не удалось записать чат {}", path.display()))?;
    Ok(())
}

pub fn list_chats() -> Result<Vec<ChatSession>> {
    let dir = chats_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut chats = Vec::new();
    for entry in
        std::fs::read_dir(&dir).with_context(|| format!("не удалось прочитать директорию {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(chat) = serde_json::from_str::<ChatSession>(&content) {
            chats.push(chat);
        }
    }
    chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(chats)
}

/// Текст, которым история другого чата переносится в текущий одним блоком.
pub fn context_block(chat: &ChatSession) -> String {
    let mut block = format!("[Контекст из чата «{}»]\n", chat.title);
    for message in &chat.messages {
        let label = match message.role {
            Role::User => "Вы",
            Role::Assistant => "Агент",
        };
        block.push_str(&format!("{label}: {}\n", message.content));
    }
    block
}

pub fn delete_chat(id: &str) -> Result<()> {
    let dir = chats_dir()?;
    let path = chat_path(&dir, id);
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("не удалось удалить чат {}", path.display()))?;
    }
    Ok(())
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
