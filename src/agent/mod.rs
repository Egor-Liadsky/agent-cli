mod http;

pub use http::HttpAgent;

use crate::config::ChatSettings;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Цепочка рассуждений модели, если модель её вернула.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Телеметрия сообщения: токены, время, скорость.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<MessageMeta>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            reasoning: None,
            meta: Some(MessageMeta {
                sent_at: Some(now_secs()),
                ..MessageMeta::default()
            }),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: None,
            meta: None,
        }
    }
}

/// Измеримые характеристики одного обмена с API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMeta {
    /// Токены запроса (весь контекст, отправленный модели).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    /// Токены ответа, включая токены рассуждения.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// Токены, потраченные именно на рассуждение, если API их отделяет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    /// Время от отправки запроса до получения ответа.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Unix-время отправки запроса.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<i64>,
    /// Unix-время получения ответа.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<i64>,
}

impl MessageMeta {
    /// Скорость генерации в токенах в секунду.
    pub fn tokens_per_second(&self) -> Option<f64> {
        let tokens = self.completion_tokens? as f64;
        let seconds = self.duration_ms? as f64 / 1000.0;
        if seconds <= 0.0 {
            return None;
        }
        Some(tokens / seconds)
    }
}

/// Ответ агента вместе с рассуждением и телеметрией.
#[derive(Debug, Clone)]
pub struct AgentReply {
    pub content: String,
    pub reasoning: Option<String>,
    pub meta: MessageMeta,
}

pub fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[async_trait]
pub trait Agent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply>;
}
