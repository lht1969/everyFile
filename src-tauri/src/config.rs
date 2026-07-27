use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub max_cache_items: usize,
    pub max_history_items: usize,
    pub index_settings: IndexSettings,
    #[serde(default)]
    pub monitored_volumes: Vec<String>,
    #[serde(default)]
    pub startup: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSettings {
    pub enable_usn_journal: bool,
    pub include_hidden_files: bool,
    pub include_system_files: bool,
    pub update_interval: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_cache_items: 50,
            max_history_items: 20,
            index_settings: IndexSettings::default(),
            monitored_volumes: vec!["D:".to_string()],
            startup: false,
        }
    }
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            enable_usn_journal: true,
            include_hidden_files: false,
            include_system_files: false,
            update_interval: 5,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path()?;

        if config_path.exists() {
            let content = fs::read_to_string(config_path)?;
            let mut config: Config = toml::from_str(&content)?;
            // 迁移：旧配置中的 update_interval 如果不是 5 秒，自动修正为 5 秒并保存
            // 这避免了用户之前保存的 2 秒轮询继续导致频繁刷新
            if config.index_settings.update_interval != 5 {
                log::info!(
                    "Config migration: update_interval {} -> 5",
                    config.index_settings.update_interval
                );
                config.index_settings.update_interval = 5;
                if let Err(e) = config.save() {
                    log::warn!("Failed to save migrated config: {}", e);
                }
            }
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = Self::config_path()?;
        log::info!("Saving config to: {:?}", config_path);

        if let Some(parent) = config_path.parent() {
            log::info!("Creating parent directory: {:?}", parent);
            fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        log::info!("Config content: {}", content);
        fs::write(config_path, content)?;
        log::info!("Config saved successfully");

        Ok(())
    }

    pub fn config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let config_dir = dirs::config_dir()
            .ok_or("Cannot find config directory")?
            .join("everyFile");

        Ok(config_dir.join("config.toml"))
    }
}
