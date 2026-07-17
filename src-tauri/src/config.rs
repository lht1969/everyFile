use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use log;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub scan_all_volumes: bool,
    pub default_volume: String,
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
            scan_all_volumes: true,
            default_volume: "D:".to_string(),
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
            update_interval: 2,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path()?;

        if config_path.exists() {
            let content = fs::read_to_string(config_path)?;
            let config: Config = toml::from_str(&content)?;
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

    fn config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let config_dir = dirs::config_dir()
            .ok_or("Cannot find config directory")?
            .join("Everything");

        Ok(config_dir.join("config.toml"))
    }
}
