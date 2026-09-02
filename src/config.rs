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

    /// Настройки формата ответа, если включён кастомный режим
    pub fn active_response_format(&self) -> Option<&ResponseFormat> {
        if self.custom_response_mode {
            Some(&self.response_format)
        } else {
            None
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
