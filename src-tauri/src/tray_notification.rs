//! 启动通知模块
//!
//! 当程序以静默模式（开机自启动）启动时，
//! 使用 tauri-plugin-notification 显示 Windows Toast 通知，
//! 明确告知用户程序已经启动并托盘位置。
//!
//! 这样用户能直观看到程序是否真的启动，
//! 解决开机自启"看不见程序运行"的问题。

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

/// 显示开机启动 Toast 通知
///
/// 应在检测到 silent_mode 时调用。
/// 由于通知需要 AppHandle，会在 Tauri setup 中延迟调用。
///
/// # 参数
/// - `app`: Tauri AppHandle，用于调用通知插件
///
/// # 注意
/// 如果通知失败（例如权限不足、用户禁用了通知），
/// 不会影响程序正常运行，仅记录警告日志。
pub fn show_startup_notification(app: &AppHandle) {
    let title = "Everything Tauri";
    let body = "已开机启动，程序在后台运行。\n\
        点击任务栏右下角托盘图标可以打开主窗口。";

    // 调用通知插件显示 Toast 通知
    let result = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .icon("icon")
        .show();

    match result {
        Ok(()) => log::info!("开机启动通知显示成功"),
        Err(e) => log::warn!("显示开机启动通知失败: {}", e),
    }
}
