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
    /// Управление конфигурацией (адрес сервиса, токен, модель)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Локальные модели через Ollama
    Ollama {
        #[command(subcommand)]
        action: OllamaAction,
    },
}

#[derive(Subcommand)]
pub enum OllamaAction {
    /// Показать локально скачанные модели (запрос к запущенному Ollama)
    Models,
    /// Сделать локальную модель моделью по умолчанию для новых чатов
    Use {
        /// Имя модели, например gemma4:26b
        model: String,
    },
    /// Задать адрес сервера Ollama (по умолчанию http://localhost:11434)
    SetUrl {
        /// Адрес без /api/chat
        url: String,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Сохранить клиентский токен сервиса в конфиг
    SetToken {
        /// Значение токена. Пустая строка — токен не отправляется
        token: String,
    },
    /// Задать модель по умолчанию для новых чатов
    SetModel {
        /// Имя модели, например deepseek-chat
        model: String,
    },
    /// Задать адрес сервиса agentd (например http://127.0.0.1:8080)
    SetUrl {
        /// Адрес сервиса без /v1/chat
        url: String,
    },
    /// Показать модели, разрешённые сервисом (GET /v1/models)
    Models,
    /// Задать провайдера по умолчанию для новых чатов: cloud | ollama
    SetProvider {
        /// cloud — облачный API, ollama — локальные модели
        provider: String,
    },
    /// Показать текущий конфиг (токен маскируется)
    Show,
    /// Настройка формата ответа по умолчанию для новых чатов (кастомный режим)
    Format {
        #[command(subcommand)]
        action: FormatAction,
    },
    /// Настройка параметров сэмплирования (temperature, top_p, top_k и т.д.)
    Sampling {
        #[command(subcommand)]
        action: SamplingAction,
    },
    /// Стратегия рассуждения по умолчанию для новых чатов
    Reasoning {
        /// default | step-by-step | prompt-craft | expert-panel
        /// (без значения — показать текущую)
        mode: Option<String>,
        /// Состав группы экспертов через запятую, например:
        /// "аналитик, инженер, критик". Пустая строка — состав по умолчанию
        #[arg(long, value_delimiter = ',')]
        experts: Option<Vec<String>>,
        /// Встроенный режим thinking у модели: auto | on | off
        #[arg(long)]
        thinking: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SamplingAction {
    /// Задать параметры сэмплирования (обновляет только переданные поля)
    Set {
        /// Температура сэмплирования (обычно 0.0 - 2.0)
        #[arg(long)]
        temperature: Option<f32>,
        /// Top-p (nucleus sampling), 0.0 - 1.0
        #[arg(long = "top-p")]
        top_p: Option<f32>,
        /// Top-k сэмплирование
        #[arg(long = "top-k")]
        top_k: Option<u32>,
        /// Штраф за частоту повторения токенов
        #[arg(long = "frequency-penalty")]
        frequency_penalty: Option<f32>,
        /// Штраф за присутствие токена в тексте
        #[arg(long = "presence-penalty")]
        presence_penalty: Option<f32>,
    },
    /// Сбросить все параметры сэмплирования (вернуться к дефолтным значениям API)
    Reset,
    /// Показать текущие параметры сэмплирования
    Show,
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
