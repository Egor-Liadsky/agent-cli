use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agentcli", about = "CLI для диалога с облачным AI-агентом")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Задать один вопрос агенту и получить ответ
    Ask {
        /// Текст вопроса
        prompt: String,
    },
    /// Начать интерактивный диалог с агентом
    Chat,
    /// Управление конфигурацией (API key, base URL, модель)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Сохранить API key в конфиг
    SetKey {
        /// Значение API key
        key: String,
    },
    /// Показать текущий конфиг (ключ маскируется)
    Show,
}
