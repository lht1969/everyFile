use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::result::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigResponse {
    pub scan_all_volumes: bool,
    pub default_volume: String,
    pub max_cache_items: usize,
    pub max_history_items: usize,
    pub enable_usn_journal: bool,
    pub include_hidden_files: bool,
    pub include_system_files: bool,
    pub update_interval: u32,
    pub monitored_volumes: Vec<String>,
    pub startup: bool,
}

impl From<Config> for ConfigResponse {
    fn from(c: Config) -> Self {
        Self {
            scan_all_volumes: c.scan_all_volumes,
            default_volume: c.default_volume,
            max_cache_items: c.max_cache_items,
            max_history_items: c.max_history_items,
            enable_usn_journal: c.index_settings.enable_usn_journal,
            include_hidden_files: c.index_settings.include_hidden_files,
            include_system_files: c.index_settings.include_system_files,
            update_interval: c.index_settings.update_interval,
            monitored_volumes: c.monitored_volumes,
            startup: c.startup,
        }
    }
}

#[tauri::command]
pub fn get_config() -> Result<ConfigResponse, String> {
    let config = Config::load().map_err(|e| e.to_string())?;
    Ok(ConfigResponse::from(config))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveConfigParams {
    pub scan_all_volumes: bool,
    pub default_volume: String,
    pub max_cache_items: usize,
    pub max_history_items: usize,
    pub enable_usn_journal: bool,
    pub include_hidden_files: bool,
    pub include_system_files: bool,
    pub update_interval: u32,
    pub monitored_volumes: Vec<String>,
    pub startup: bool,
}

#[tauri::command]
pub fn save_config(params: SaveConfigParams) -> Result<(), String> {
    println!("Received save_config request: {:?}", params);
    
    let config = Config {
        scan_all_volumes: params.scan_all_volumes,
        default_volume: params.default_volume,
        max_cache_items: params.max_cache_items,
        max_history_items: params.max_history_items,
        index_settings: crate::config::IndexSettings {
            enable_usn_journal: params.enable_usn_journal,
            include_hidden_files: params.include_hidden_files,
            include_system_files: params.include_system_files,
            update_interval: params.update_interval,
        },
        monitored_volumes: params.monitored_volumes,
        startup: params.startup,
    };
    
    println!("Saving config: {:?}", config);
    
    // 保存配置
    match config.save() {
        Ok(_) => {
            println!("Config saved successfully");
            Ok(())
        },
        Err(e) => {
            println!("Failed to save config: {:?}", e);
            Err(e.to_string())
        }
    }
}
