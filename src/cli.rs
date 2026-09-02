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
    /// Настройка формата ответа (второй, кастомный режим)
    Format {
        #[command(subcommand)]
        action: FormatAction,
    },
}

#[derive(Subcommand)]
pub enum FormatAction {
    /// Задать параметры формата ответа (обновляет только переданные поля)
    Set {
        /// Описание формата ответа, например: "отвечай маркированным списком"
        #[arg(long)]
        description: Option<String>,
        /// Максимальная длина ответа в токенах
        #[arg(long = "max-length")]
        max_length: Option<u32>,
        /// Stop-последовательности через запятую
        #[arg(long, value_delimiter = ',')]
        stop: Option<Vec<String>>,
        /// Явная инструкция модели о том, когда завершать ответ
        #[arg(long = "stop-instruction")]
        stop_instruction: Option<String>,
    },
    /// Включить кастомный режим ответа
    Enable,
    /// Выключить кастомный режим ответа (вернуться к дефолтному)
    Disable,
    /// Сбросить все настройки формата
    Reset,
    /// Показать текущие настройки формата
    Show,
}
