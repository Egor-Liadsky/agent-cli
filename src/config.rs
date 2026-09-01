use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Config {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
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

    pub fn masked_api_key(&self) -> String {
        match &self.api_key {
            None => "<не задан>".to_string(),
            Some(key) if key.len() <= 8 => "*".repeat(key.len()),
            Some(key) => format!("{}***{}", &key[..4], &key[key.len() - 4..]),
        }
    }
}
