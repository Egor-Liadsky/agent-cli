use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct ResponseFormat {
    /// Описание формата ответа (например: "отвечай маркированным списком")
    pub description: Option<String>,
    /// Ограничение на длину ответа в токенах (max_tokens)
    pub max_length: Option<u32>,
    /// Stop-последовательности: API оборвёт ответ, встретив одну из них
    pub stop: Option<Vec<String>>,
    /// Явная инструкция модели о том, когда завершать ответ
    pub stop_instruction: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SamplingParams {
    /// Температура сэмплирования (обычно 0.0 - 2.0)
    pub temperature: Option<f32>,
    /// Top-p (nucleus sampling), 0.0 - 1.0
    pub top_p: Option<f32>,
    /// Top-k сэмплирование
    pub top_k: Option<u32>,
    /// Штраф за частоту повторения токенов
    pub frequency_penalty: Option<f32>,
    /// Штраф за присутствие токена в тексте
    pub presence_penalty: Option<f32>,
}

/// Стратегия рассуждения агента: подмешивается в системный промпт чата.
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningMode {
    /// Без дополнительных инструкций — обычный ответ модели
    #[default]
    Default,
    /// Пошаговое решение задачи с явными шагами и выводом
    StepByStep,
    /// Сначала составить качественный промпт для решения задачи, потом решить по нему
    PromptCraft,
    /// Группа экспертов (аналитик, инженер, критик) обсуждает задачу и даёт общий ответ
    ExpertPanel,
}

impl ReasoningMode {
    pub const ALL: [ReasoningMode; 4] = [
        ReasoningMode::Default,
        ReasoningMode::StepByStep,
        ReasoningMode::PromptCraft,
        ReasoningMode::ExpertPanel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ReasoningMode::Default => "По умолчанию",
            ReasoningMode::StepByStep => "Решать пошагово",
            ReasoningMode::PromptCraft => "Составить промпт для решения",
            ReasoningMode::ExpertPanel => "Группа экспертов",
        }
    }

    /// Состав группы экспертов по умолчанию, если пользователь не задал свой.
    pub const DEFAULT_EXPERTS: [&'static str; 3] = [
        "аналитик (уточняет постановку и риски)",
        "инженер (предлагает конкретное решение)",
        "критик (ищет слабые места и предлагает правки)",
    ];

    /// Часть системного промпта для выбранной стратегии.
    /// `experts` учитывается только для режима «Группа экспертов»;
    /// пустой список означает состав по умолчанию.
    pub fn system_prompt(self, experts: &[String]) -> Option<String> {
        match self {
            ReasoningMode::Default => None,
            ReasoningMode::StepByStep => Some(
                "Решай задачу пошагово. Сначала разбей её на пронумерованные шаги, \
                 выполни каждый шаг по порядку, показывая промежуточные выводы, \
                 затем дай итоговый ответ отдельным блоком «Итог»."
                    .to_string(),
            ),
            ReasoningMode::PromptCraft => Some(
                "Сначала составь подробный промпт, который лучше всего описывает задачу: \
                 цель, контекст, ограничения, критерии хорошего решения и формат ответа. \
                 Покажи этот промпт в блоке «Промпт», затем реши задачу по нему \
                 и дай ответ в блоке «Решение»."
                    .to_string(),
            ),
            ReasoningMode::ExpertPanel => {
                let roles: Vec<String> = if experts.is_empty() {
                    Self::DEFAULT_EXPERTS.iter().map(|e| e.to_string()).collect()
                } else {
                    experts.to_vec()
                };
                Some(format!(
                    "Разбери задачу как группа экспертов: {}. Покажи короткую реплику \
                     каждого эксперта под его именем, затем дай согласованный итоговый \
                     ответ в блоке «Итог».",
                    roles.join(", ")
                ))
            }
        }
    }

    /// Разбор значения из CLI.
    pub fn parse(value: &str) -> Option<ReasoningMode> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "default" | "по-умолчанию" => Some(ReasoningMode::Default),
            "step-by-step" | "steps" | "пошагово" => Some(ReasoningMode::StepByStep),
            "prompt-craft" | "prompt" | "промпт" => Some(ReasoningMode::PromptCraft),
            "expert-panel" | "experts" | "эксперты" => Some(ReasoningMode::ExpertPanel),
            _ => None,
        }
    }
}

/// Параметры агента, привязанные к конкретному чату.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct ChatSettings {
    /// Режим ответа: false — дефолтный, true — кастомный (см. response_format)
    #[serde(default)]
    pub custom_response_mode: bool,
    #[serde(default)]
    pub response_format: ResponseFormat,
    #[serde(default)]
    pub sampling: SamplingParams,
    /// Стратегия рассуждения агента для этого чата
    #[serde(default)]
    pub reasoning: ReasoningMode,
    /// Состав группы экспертов для режима «Группа экспертов».
    /// Пустой список — состав по умолчанию (аналитик, инженер, критик).
    #[serde(default)]
    pub experts: Vec<String>,
}

impl ChatSettings {
    /// Системный промпт выбранной стратегии рассуждения
    pub fn reasoning_prompt(&self) -> Option<String> {
        self.reasoning.system_prompt(&self.experts)
    }

    /// Настройки формата ответа, если включён кастомный режим
    pub fn active_response_format(&self) -> Option<&ResponseFormat> {
        if self.custom_response_mode {
            Some(&self.response_format)
        } else {
            None
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Config {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Режим ответа: false — дефолтный, true — кастомный (см. response_format)
    #[serde(default)]
    pub custom_response_mode: bool,
    #[serde(default)]
    pub response_format: ResponseFormat,
    /// Параметры сэмплирования модели (temperature, top_p, top_k и т.д.)
    #[serde(default)]
    pub sampling: SamplingParams,
    /// Стратегия рассуждения по умолчанию для новых чатов
    #[serde(default)]
    pub reasoning: ReasoningMode,
    /// Состав группы экспертов по умолчанию для новых чатов
    #[serde(default)]
    pub experts: Vec<String>,
}

impl Config {
    fn path() -> Result<PathBuf> {
        let dir = dirs::config_dir().context("не удалось определить домашнюю директорию конфигов")?;
        Ok(dir.join("agentcli").join("config.toml"))
    }

    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("не удалось прочитать конфиг {}", path.display()))?;
        let config = toml::from_str(&content)
            .with_context(|| format!("не удалось разобрать конфиг {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("не удалось создать директорию {}", parent.display()))?;
        }
        let content = toml::to_string_pretty(self).context("не удалось сериализовать конфиг")?;
        std::fs::write(&path, content)
            .with_context(|| format!("не удалось записать конфиг {}", path.display()))?;
        Ok(())
    }

    /// Значения по умолчанию для новых чатов: глобальный конфиг служит
    /// шаблоном, дальше каждый чат правит свои параметры независимо.
    pub fn default_chat_settings(&self) -> ChatSettings {
        ChatSettings {
            custom_response_mode: self.custom_response_mode,
            response_format: self.response_format.clone(),
            sampling: self.sampling.clone(),
            reasoning: self.reasoning,
            experts: self.experts.clone(),
        }
    }

    pub fn masked_api_key(&self) -> String {
        match &self.api_key {
            None => "<не задан>".to_string(),
            Some(key) if key.len() <= 8 => "*".repeat(key.len()),
            Some(key) => format!("{}***{}", &key[..4], &key[key.len() - 4..]),
        }
    }
}
