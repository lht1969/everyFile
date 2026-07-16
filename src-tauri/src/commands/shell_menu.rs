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
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    IShellItem, IContextMenu3,
    SHCreateItemFromParsingName, CMF_NORMAL, CMINVOKECOMMANDINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, TrackPopupMenuEx, TPM_RETURNCMD, TPM_LEFTALIGN, TPM_TOPALIGN,
    DestroyMenu, SetForegroundWindow,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::core::{PCSTR, PCWSTR};

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
    // 诊断日志：Tauri command 入口，确认 Rust 端被调用，记录路径与屏幕坐标
    log::info!(
        "[CTX_MENU] show_context_menu called: path={}, screen=({}, {})",
        path,
        screen_x,
        screen_y
    );

    // 1. 拿到主窗口 HWND
    // Tauri 2 通过 windows 0.61 的 HWND 返回（pub struct HWND(pub *mut c_void)），
    // 而本项目使用 windows 0.52（pub struct HWND(pub isize)），
    // 两个版本的 HWND 类型不同，需要 .0 as isize 桥接。
    let window: WebviewWindow = match app.get_webview_window("main") {
        Some(w) => {
            log::info!("[CTX_MENU] Got main window");
            w
        }
        None => {
            log::error!("[CTX_MENU] Main window not found");
            return Err("Main window not found".to_string());
        }
    };
    let hwnd_tauri = match window.hwnd() {
        Ok(h) => {
            log::info!("[CTX_MENU] Got hwnd (Tauri HWND): {:?}", h);
            h
        }
        Err(e) => {
            log::error!("[CTX_MENU] Failed to get hwnd: {}", e);
            return Err(format!("Failed to get hwnd: {}", e));
        }
    };
    let hwnd = HWND(hwnd_tauri.0 as isize);
    log::info!("[CTX_MENU] Bridged to windows 0.52 HWND: {:?}", hwnd);

    // 2. spawn_blocking 避免污染 tokio 运行时
    let result = tokio::task::spawn_blocking(move || {
        log::info!("[CTX_MENU] spawn_blocking thread started");
        let r = show_menu_blocking(hwnd, &path, screen_x, screen_y);
        log::info!(
            "[CTX_MENU] spawn_blocking thread finished: {:?}",
            r.as_ref().map(|_| "ok").map_err(|e| e.as_str())
        );
        r
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(e) => {
            log::error!("[CTX_MENU] Join error: {}", e);
            Err(format!("Join error: {}", e))
        }
    }
}

/// 在独立线程中执行菜单弹出（COM 初始化/反初始化在此完成）
fn show_menu_blocking(hwnd: HWND, path: &str, x: i32, y: i32) -> Result<(), String> {
    // 3. 初始化 COM（STA 模式 + 禁用旧 OLE）
    // windows 0.52 中 CoInitializeEx 返回 Result<()>，不再需要 .ok() 转 Option
    // 诊断日志：CoInitializeEx 在已经初始化的线程上会返回 S_FALSE（视为 Err），
    // 这里只警告不返回错误，避免误判为致命失败
    unsafe {
        match CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) {
            Ok(()) => log::info!("[CTX_MENU] CoInitializeEx succeeded"),
            Err(e) => {
                log::warn!(
                    "[CTX_MENU] CoInitializeEx returned error (may be already initialized): {}",
                    e
                );
                // 不返回错误，可能是 S_FALSE（已初始化）
            }
        }
    }

    let result = (|| -> Result<(), String> {
        // 4. 路径 → IShellItem
        // 诊断日志：即将创建 IShellItem（失败通常意味着路径非法/不存在/无权限）
        let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        log::info!("[CTX_MENU] Creating IShellItem for path: {}", path);
        let item: IShellItem = unsafe {
            SHCreateItemFromParsingName(PCWSTR(path_w.as_ptr()), None).map_err(|e| {
                log::error!("[CTX_MENU] SHCreateItemFromParsingName failed: {}", e);
                format!("SHCreateItemFromParsingName failed: {}", e)
            })?
        };
        log::info!("[CTX_MENU] IShellItem created successfully");

        // 5. IShellItem → IContextMenu3
        // 诊断日志：BindToHandler 失败通常意味着 shell 扩展未注册或权限不足
        log::info!("[CTX_MENU] Calling BindToHandler with BHID_CONTEXT_MENU");
        let ctx_menu: IContextMenu3 = unsafe {
            item.BindToHandler(None, &BHID_CONTEXT_MENU).map_err(|e| {
                log::error!("[CTX_MENU] BindToHandler failed: {}", e);
                format!("BindToHandler failed: {}", e)
            })?
        };
        log::info!("[CTX_MENU] BindToHandler returned IContextMenu (cast to IContextMenu3)");

        // 6. 创建弹出菜单
        // windows 0.52 中 CreatePopupMenu 返回 Result<HMENU, Error>，用 ? 直接解包
        // 诊断日志：记录 HMENU 句柄值，便于追踪
        log::info!("[CTX_MENU] Creating popup menu");
        let menu = unsafe { CreatePopupMenu() }.map_err(|e| {
            log::error!("[CTX_MENU] CreatePopupMenu failed: {}", e);
            format!("CreatePopupMenu failed: {}", e)
        })?;
        log::info!("[CTX_MENU] Popup menu created: HMENU={:?}", menu);

        // 7. 填充菜单项
        // windows 0.52 中 CMF_NORMAL 是 pub const CMF_NORMAL: u32，直接传值
        // 诊断日志：QueryContextMenu 失败意味着 shell 扩展注册时失败或路径不可读
        log::info!("[CTX_MENU] Calling QueryContextMenu");
        unsafe {
            ctx_menu
                .QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL)
                .map_err(|e| {
                    log::error!("[CTX_MENU] QueryContextMenu failed: {}", e);
                    format!("QueryContextMenu failed: {}", e)
                })?;
        }
        log::info!("[CTX_MENU] QueryContextMenu succeeded");

        // 8. 弹出菜单（设置前置窗口确保 Z-order 正确）
        // windows 0.52 中 TrackPopupMenuEx 的 uflags 参数是 u32，
        // 而 TPM_* 常量是 TRACK_POPUP_MENU_FLAGS 类型（#[repr(transparent)] pub struct(pub u32)），
        // 用 .0 取底层 u32 后按位或组合
        // 诊断日志：记录弹出坐标。如果菜单"无反应"是出现在这里，说明菜单没显示
        log::info!("[CTX_MENU] Calling TrackPopupMenuEx at ({}, {})", x, y);
        let cmd = unsafe {
            SetForegroundWindow(hwnd);
            TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD.0 | TPM_LEFTALIGN.0 | TPM_TOPALIGN.0,
                x,
                y,
                hwnd,
                None,
            )
        };
        log::info!("[CTX_MENU] TrackPopupMenuEx returned: cmd={}", cmd.0);

        // 9. 销毁菜单句柄
        // 诊断日志：菜单销毁前的最终记录点
        unsafe { DestroyMenu(menu).ok(); }
        log::info!("[CTX_MENU] Menu destroyed");

        // 10. 用户选中某项则执行命令
        // TPM_RETURNCMD 模式下 TrackPopupMenuEx 返回的 cmd 是从 QueryContextMenu 时
        // 传入的 idFirst(=1) 开始的相对 menu item ID。Windows 资源管理器调用
        // IContextMenu3::InvokeCommand 时，lpVerb 传的是 MAKEINTRESOURCE 形式的 verb
        // 偏移（即 cmd - idFirst），这是 Windows shell context menu 的标准调用约定。
        if cmd.0 != 0 {
            // 诊断日志：用户确实选了某项，准备执行
            log::info!("[CTX_MENU] User selected menu item, invoking command");
            // 因为 QueryContextMenu 时 idFirst=1，cmd 减 1 才是 verb index
            let verb_index: i32 = cmd.0 - 1;
            // windows crate 0.52 中 CMINVOKECOMMANDINFO 的 lpVerb 字段类型是 PCSTR
            // （pub struct PCSTR(pub *const u8)），与 Win32 SDK 的 LPCSTR 对应。
            // 通过把 verb_index cast 为 usize 再 cast 为 *const u8 模拟 MAKEINTRESOURCE：
            // Windows API 收到该"指针"时，会检查低 16 位非零，把它当 WORD 整数 ID 解释
            // （IContextMenu3 内部会与 idFirst 相减得到 verb 偏移）。
            let lp_verb = PCSTR(verb_index as usize as *const u8);
            // 构造 CMINVOKECOMMANDINFO 结构体
            // - cbSize 必须正确设置
            // - fMask 留 0（不需要扩展信息）
            // - hwnd 透传主窗口句柄
            // - nShow = 1 对应 SW_SHOWNORMAL
            // - 其余字段 Default::default() 即可（lpParameters/lpDirectory 留 null，
            //   dwHotKey 留 0，hIcon 留 null）
            let invoke = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                fMask: 0,
                hwnd,
                lpVerb: lp_verb,
                lpParameters: PCSTR::null(),
                lpDirectory: PCSTR::null(),
                nShow: 1, // SW_SHOWNORMAL
                dwHotKey: 0,
                hIcon: windows::Win32::Foundation::HANDLE(0),
            };
            // IContextMenu3::InvokeCommand 接受 *const CMINVOKECOMMANDINFO 指针
            unsafe {
                ctx_menu.InvokeCommand(&invoke).map_err(|e| {
                    log::error!("[CTX_MENU] InvokeCommand failed: {}", e);
                    format!("InvokeCommand failed: {}", e)
                })?;
            }
            log::info!("[CTX_MENU] Command invoked successfully");
        } else {
            // 诊断日志：cmd=0 通常意味着用户按 Esc 或点击菜单外区域关闭菜单
            log::info!("[CTX_MENU] User dismissed menu (cmd=0)");
        }

        Ok(())
    })();

    // 11. 反初始化 COM
    // 诊断日志：COM 释放前的最终状态记录（result 是 Ok/Err）
    log::info!(
        "[CTX_MENU] About to CoUninitialize, inner result: {:?}",
        result.as_ref().map(|_| "ok").map_err(|e| e.as_str())
    );
    unsafe { CoUninitialize(); }
    result
}
