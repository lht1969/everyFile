use crate::fs;
use winreg::enums::*;
use winreg::RegKey;

#[tauri::command]
pub fn is_admin() -> bool {
    fs::is_elevated()
}

fn startup_key_path() -> &'static str {
    "Software\\Microsoft\\Windows\\CurrentVersion\\Run"
}

#[tauri::command]
pub fn add_startup() -> std::result::Result<(), String> {
    let exe_path = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {}", e))?;
    let exe_path_str = exe_path.to_str().ok_or("程序路径转换失败".to_string())?;
    let command = format!("\"{}\" -s", exe_path_str);
    log::info!("Adding startup entry: {}", command);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(startup_key_path())
        .map_err(|e| format!("打开注册表 HKCU\\{} 失败: {}", startup_key_path(), e))?;

    // 设置前先读取旧值
    let old_value: Result<String, _> = key.get_value("everyFile");
    match old_value {
        Ok(ref val) if val == &command => {
            log::info!("Startup entry already up-to-date, skipping");
            return Ok(());
        }
        Ok(ref val) => {
            log::info!("Updating startup entry from '{}' to '{}'", val, command);
        }
        Err(_) => {
            log::info!("Creating new startup entry");
        }
    }

    key.set_value("everyFile", &command)
        .map_err(|e| format!("设置注册表值 '{}' 失败: {}", command, e))?;

    // 验证写入结果
    let verify: Result<String, _> = key.get_value("everyFile");
    match verify {
        Ok(ref val) if val == &command => {
            log::info!("Startup entry verified successfully");
        }
        Ok(val) => {
            log::warn!(
                "Startup entry mismatch: wrote '{}', read '{}'",
                command,
                val
            );
        }
        Err(e) => {
            log::error!("Failed to verify startup entry: {}", e);
        }
    }

    log::info!("Startup entry added successfully");
    Ok(())
}

#[tauri::command]
pub fn remove_startup() -> std::result::Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(startup_key_path(), KEY_WRITE)
        .map_err(|e| format!("打开注册表 HKCU\\{} 失败: {}", startup_key_path(), e))?;

    match key.delete_value("everyFile") {
        Ok(_) => {
            log::info!("Startup entry removed successfully");
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to remove startup entry: {}", e);
            Err(format!("删除注册表值失败: {}", e))
        }
    }
}

#[tauri::command]
pub fn is_startup_enabled() -> std::result::Result<bool, String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey(startup_key_path()) {
        Ok(k) => k,
        Err(e) => {
            log::debug!("Failed to open Run key: {}", e);
            return Ok(false);
        }
    };
    match key.get_value::<String, _>("everyFile") {
        Ok(val) => {
            log::info!("Startup entry found: {}", val);
            Ok(true)
        }
        Err(_) => {
            log::info!("No startup entry found");
            Ok(false)
        }
    }
}
