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
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("获取程序路径失败: {}", e))?;
    let exe_path_str = exe_path.to_str()
        .ok_or("程序路径转换失败".to_string())?;
    let command = format!("\"{}\" -s", exe_path_str);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(startup_key_path())
        .map_err(|e| format!("打开注册表失败: {}", e))?;
    key.set_value("Everything Tauri", &command)
        .map_err(|e| format!("设置注册表值失败: {}", e))?;
    Ok(())
}

#[tauri::command]
pub fn remove_startup() -> std::result::Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(startup_key_path())
        .map_err(|e| format!("打开注册表失败: {}", e))?;
    key.delete_value("Everything Tauri")
        .map_err(|e| format!("删除注册表值失败: {}", e))?;
    Ok(())
}

#[tauri::command]
pub fn is_startup_enabled() -> std::result::Result<bool, String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey(startup_key_path()) {
        Ok(k) => k,
        Err(_) => return Ok(false),
    };
    let value: Result<String, _> = key.get_value("Everything Tauri");
    Ok(value.is_ok())
}
