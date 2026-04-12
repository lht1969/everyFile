use crate::fs;
use std::path::Path;
use winreg::enums::*;
use winreg::RegKey;

#[tauri::command]
pub fn is_admin() -> bool {
    fs::is_elevated()
}

#[tauri::command]
pub fn add_startup() -> std::result::Result<(), String> {
    // 获取程序的完整路径
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("获取程序路径失败: {}", e))?;
    
    let exe_path_str = exe_path.to_str()
        .ok_or("程序路径转换失败".to_string())?;
    
    // 构建启动命令，添加 -s 参数
    let command = format!("\"{}\" -s", exe_path_str);
    
    // 打开注册表项（使用 HKEY_LOCAL_MACHINE 以管理员身份启动）
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (key, _) = hklm.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|e| format!("打开注册表失败: {}", e))?;
    
    // 设置注册表值
    key.set_value("Everything Tauri", &command)
        .map_err(|e| format!("设置注册表值失败: {}", e))?;
    
    Ok(())
}

#[tauri::command]
pub fn remove_startup() -> std::result::Result<(), String> {
    // 打开注册表项
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|e| format!("打开注册表失败: {}", e))?;
    
    // 删除注册表值
    key.delete_value("Everything Tauri")
        .map_err(|e| format!("删除注册表值失败: {}", e))?;
    
    Ok(())
}

#[tauri::command]
pub fn is_startup_enabled() -> std::result::Result<bool, String> {
    // 打开注册表项
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|e| format!("打开注册表失败: {}", e))?;
    
    // 检查注册表值是否存在
    let value: Result<String, _> = key.get_value("Everything Tauri");
    Ok(value.is_ok())
}
