use super::yellowstone::config::YellowstoneConfig;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub yellowstone: YellowstoneConfig,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let path =
            std::env::var("CONFIG_PATH").unwrap_or_else(|_| "configs/local/bot.yaml".to_string());
        Ok(serde_norway::from_str(&std::fs::read_to_string(&path)?)?)
    }
}
