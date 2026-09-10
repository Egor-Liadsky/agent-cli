//! Выбор агента по провайдеру чата.
//!
//! Диспетчеризация живёт у потребителя ядра, а не внутри одной реализации
//! `Agent`: так набор доступных провайдеров задаётся графом зависимостей
//! приложения, а не общим для всех типом. Облачного провайдера в этом наборе
//! нет: `Provider::Cloud` означает запрос к сервису `agentd`, а прямой вызов
//! провайдера клиенту недоступен — крейта с ним нет в его зависимостях.

use agentclient::ServerAgent;
use agentcore::agent::{Agent, AgentReply, Message, OllamaAgent};
use agentcore::config::{ChatSettings, Config, Provider};
use agentcore::logging::ExchangeLog;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

pub struct CliAgent {
    server: ServerAgent,
    local: OllamaAgent,
}

impl CliAgent {
    pub fn from_config(config: &Config, log: Arc<ExchangeLog>) -> Result<Self> {
        Ok(Self {
            server: ServerAgent::new(
                config.effective_server_url(),
                config.client_token(),
                config.model.clone().unwrap_or_default(),
                log.clone(),
            ),
            local: OllamaAgent::from_config(config, log)?,
        })
    }

    pub fn with_unauthorized_hint(mut self, hint: impl Into<String>) -> Self {
        self.server = self.server.with_unauthorized_hint(hint);
        self
    }
}

impl CliAgent {
    /// Ответ в чате, который хранится в сервисе. Облачный чат идёт запросом
    /// с `chat_id`: сервис сам берёт историю и записывает обмен. Локальная
    /// модель отвечает по истории, переданной клиентом, а обмен клиент
    /// дозаписывает в сервис отдельно (specs/client-chat-storage).
    pub async fn ask_in_chat(
        &self,
        chat_id: &str,
        prompt: &str,
        history: &[Message],
        settings: &ChatSettings,
    ) -> Result<AgentReply> {
        match settings.provider {
            Provider::Cloud => self.server.ask_in_chat(chat_id, prompt, settings).await,
            Provider::Ollama => self.local.ask(history, settings).await,
        }
    }
}

#[async_trait]
impl Agent for CliAgent {
    async fn ask(&self, history: &[Message], settings: &ChatSettings) -> Result<AgentReply> {
        match settings.provider {
            Provider::Cloud => self.server.ask(history, settings).await,
            Provider::Ollama => self.local.ask(history, settings).await,
        }
    }
}
