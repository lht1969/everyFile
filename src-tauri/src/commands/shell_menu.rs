// 集成 Windows Shell 原生右键菜单
//
// 功能：
// - 通过 SHCreateItemFromParsingName 从路径创建 IShellItem
// - 调用 IContextMenu3::QueryContextMenu 填充弹出菜单
// - TrackPopupMenuEx 弹出与资源管理器一致的菜单（含第三方扩展）
// - 用户选择命令后调用 IContextMenu3::InvokeCommand 执行
//
// 多线程模型：
// - Tauri command 用 spawn_blocking 跑在独立线程
// - 该线程初始化 COM(STA)，独立消息循环
//
// 依赖：windows crate 0.52 已开启 Win32_UI_Shell / Win32_System_Com / Win32_UI_WindowsAndMessaging

use tauri::{AppHandle, Manager, WebviewWindow};
use windows::core::Interface;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::{
    IShellItem, IContextMenu3, IContextMenu2, IContextMenu,
    SHCreateItemFromParsingName, CMF_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, TrackPopupMenuEx, TPM_RETURNCMD, TPM_LEFTALIGN, TPM_TOPALIGN,
    DestroyMenu, SetForegroundWindow,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::core::PCWSTR;

/// BHID_ContextMenu：用于 BindToHandler 直接拿到 IContextMenu 接口
/// GUID 值来自 Windows SDK shobjidl.h
const BHID_CONTEXT_MENU: windows::core::GUID = windows::core::GUID::from_u128(
    0x0001f74c_7365_4940_afe9_2cf4733c8b9a_u128
);

/// 弹出系统 Shell 右键菜单
///
/// # 参数
/// - `app`: Tauri AppHandle，用于获取主窗口
/// - `path`: 目标文件/目录的完整路径（UTF-8）
/// - `screen_x`, `screen_y`: 鼠标点击位置的屏幕坐标（来自前端 onContextMenu.clientX/Y 加上 window.screenX/Y）
///
/// # 错误
/// - 主窗口不存在、COM 初始化失败、路径无效、菜单创建/弹出失败
#[tauri::command]
pub async fn show_context_menu(
    app: AppHandle,
    path: String,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    // 1. 拿到主窗口 HWND（Tauri 2 公开 API）
    let window: WebviewWindow = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;
    let hwnd_raw = window.hwnd().map_err(|e| format!("Failed to get hwnd: {}", e))?.0 as isize;
    let hwnd = HWND(hwnd_raw);

    // 2. spawn_blocking 避免污染 tokio 运行时
    let result = tokio::task::spawn_blocking(move || {
        show_menu_blocking(hwnd, &path, screen_x, screen_y)
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(e) => Err(format!("Join error: {}", e)),
    }
}

/// 在独立线程中执行菜单弹出（COM 初始化/反初始化在此完成）
fn show_menu_blocking(hwnd: HWND, path: &str, x: i32, y: i32) -> Result<(), String> {
    // 3. 初始化 COM（STA 模式 + 禁用旧 OLE）
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
            .ok()
            .map_err(|e| format!("CoInitializeEx failed: {}", e))?;
    }

    let result = (|| -> Result<(), String> {
        // 4. 路径 → IShellItem
        let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let item: IShellItem = unsafe {
            SHCreateItemFromParsingName(PCWSTR(path_w.as_ptr()), None)
                .map_err(|e| format!("SHCreateItemFromParsingName failed: {}", e))?
        };

        // 5. IShellItem → IContextMenu3
        let ctx_menu: IContextMenu3 = unsafe {
            item.BindToHandler(None, &BHID_CONTEXT_MENU)
                .map_err(|e| format!("BindToHandler failed: {}", e))?
        };

        // 6. 创建弹出菜单
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_invalid() {
            return Err("CreatePopupMenu failed".to_string());
        }

        // 7. 填充菜单项
        unsafe {
            ctx_menu
                .QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL.0 as u32)
                .map_err(|e| format!("QueryContextMenu failed: {}", e))?;
        }

        // 8. 弹出菜单（设置前置窗口确保 Z-order 正确）
        let cmd = unsafe {
            SetForegroundWindow(hwnd);
            TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                x,
                y,
                hwnd,
                None,
            )
        };

        // 9. 销毁菜单句柄
        unsafe { DestroyMenu(menu).ok(); }

        // 10. 用户选中某项则执行（InvokeCommand 实现在 Task 3）
        if cmd.0 != 0 {
            // 占位：Task 3 会补充完整 CMINVOKECOMMANDINFO 调用
        }

        Ok(())
    })();

    // 11. 反初始化 COM
    unsafe { CoUninitialize(); }
    result
}
