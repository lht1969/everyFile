use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tauri::Manager;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::search::AppState;

static CTX_MENU_CMD: AtomicI32 = AtomicI32::new(0);
static mut CTX_MENU_ICM2: usize = 0;

/// Handle of the owner window of the currently active menu session (0 = none).
/// Only one context menu may be open at a time; a new right-click cancels the
/// previous menu instead of stacking a second one on top of it.
static ACTIVE_OWNER_HWND: AtomicUsize = AtomicUsize::new(0);

/// 记录进入 Hung 状态的时间戳（Unix 秒），0 表示未记录。
/// 超过 HUNG_RECOVERY_SECONDS 后自动重置为 Idle，允许重新尝试 Shell 菜单，
/// 避免休眠/唤醒后永久卡在 fallback 菜单。
static HUNG_SINCE: AtomicI64 = AtomicI64::new(0);
const HUNG_RECOVERY_SECONDS: i64 = 300; // 5 分钟后自动恢复

/// fallback 菜单的 TrackPopupMenu 超时时间（秒）。
/// 超时后向 owner 窗口发送 WM_CANCELMODE 强制关闭菜单，
/// 防止休眠/唤醒后 TrackPopupMenu 永不返回导致前端 invoke 无响应。
const FALLBACK_MENU_TIMEOUT_SECS: u64 = 15;

/// Lifecycle of the current menu session.
///
/// Only `Hung` is used for gating: once a Shell menu build times out (the
/// hung-shell-component behaviour observed after sleep/resume), every later
/// right-click shows the built-in fallback menu instead of spawning yet
/// another thread that gets stuck inside `QueryContextMenu`.
#[repr(i32)]
enum SessionState {
    Idle = 0,
    Building = 1,
    Tracking = 2,
    Hung = 3,
}
static SESSION_STATE: AtomicI32 = AtomicI32::new(SessionState::Idle as i32);

/// How long the Shell menu may take to build (QueryContextMenu) before we
/// treat it as hung. Normal builds complete in tens of milliseconds.
const MENU_BUILD_TIMEOUT_MS: u64 = 5000;

/// `CMIC_MASK_UNICODE` — prefer the `W` fields of `CMINVOKECOMMANDINFOEX`.
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;

/// Show Windows native Shell context menu at specified screen coordinates
///
/// The whole menu session runs on a dedicated thread that owns its own hidden
/// owner window. This is required for two reasons:
/// 1. `TrackPopupMenu` must be called from the thread that owns the owner
///    window, otherwise the menu is silently not displayed.
/// 2. A sync `#[tauri::command]` executes on the main thread, and a hung Shell
///    component (observed after sleep/resume) kept `TrackPopupMenu`'s modal
///    loop stuck forever, freezing both the main window and the tray.
#[tauri::command]
pub async fn show_context_menu(
    path: String,
    screen_x: i32,
    screen_y: i32,
    app: tauri::AppHandle,
) -> std::result::Result<(), String> {
    // If a previous Shell build hung (e.g. after sleep/resume), don't spawn
    // more threads that will hang again — go straight to the fallback menu.
    if SESSION_STATE.load(Ordering::SeqCst) == SessionState::Hung as i32 {
        // 检查 Hung 状态是否已超过恢复时间，若是则重置为 Idle，重新尝试 Shell 菜单。
        // 这样休眠/唤醒后 Shell 子系统恢复正常时，无需重启程序即可恢复原生菜单。
        let hung_since = HUNG_SINCE.load(Ordering::SeqCst);
        if hung_since > 0 {
            let now = chrono::Local::now().timestamp();
            let elapsed = now - hung_since;
            if elapsed > HUNG_RECOVERY_SECONDS {
                log::info!(
                    "[CTX_MENU] Hung state expired ({}s elapsed), resetting to try Shell menu again",
                    elapsed
                );
                SESSION_STATE.store(SessionState::Idle as i32, Ordering::SeqCst);
                HUNG_SINCE.store(0, Ordering::SeqCst);
                // 继续走下方的正常 Shell 菜单流程
            } else {
                log::info!(
                    "[CTX_MENU] Shell menu known-hung ({}s ago), using built-in menu",
                    elapsed
                );
                return show_fallback_menu_async(path, screen_x, screen_y, app).await;
            }
        } else {
            log::info!("[CTX_MENU] Shell menu known-hung, using built-in menu");
            return show_fallback_menu_async(path, screen_x, screen_y, app).await;
        }
    }

    // The session thread signals us as soon as QueryContextMenu finished and
    // the native menu is about to be tracked.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
    SESSION_STATE.store(SessionState::Building as i32, Ordering::SeqCst);
    // Clone path 和 app 给 session 线程，原始 path/app 保留给超时后的 fallback 调用
    let path_for_session = path.clone();
    let app_for_session = app.clone();
    std::thread::spawn(move || {
        let _ = run_menu_session(path_for_session, screen_x, screen_y, &app_for_session, ready_tx);
    });

    match tokio::time::timeout(
        std::time::Duration::from_millis(MENU_BUILD_TIMEOUT_MS),
        ready_rx,
    )
    .await
    {
        // Menu built and is being shown by the session thread.
        Ok(Ok(Ok(()))) => Ok(()),
        // The session reported a quick, real error (bad path, COM failure...).
        Ok(Ok(Err(e))) => {
            SESSION_STATE.store(SessionState::Idle as i32, Ordering::SeqCst);
            Err(e)
        }
        Ok(Err(_)) => {
            SESSION_STATE.store(SessionState::Idle as i32, Ordering::SeqCst);
            Err("Context menu task failed".to_string())
        }
        // QueryContextMenu is stuck (hung shell component after sleep). The
        // stuck thread leaks — we cannot kill a blocked COM call — but we mark
        // the session Hung so every later right-click uses the fallback menu.
        Err(_elapsed) => {
            log::warn!(
                "[CTX_MENU] Shell menu build timed out after {}ms, marking hung and using built-in menu",
                MENU_BUILD_TIMEOUT_MS
            );
            SESSION_STATE.store(SessionState::Hung as i32, Ordering::SeqCst);
            HUNG_SINCE.store(chrono::Local::now().timestamp(), Ordering::SeqCst);
            show_fallback_menu_async(path, screen_x, screen_y, app).await
        }
    }
}

fn run_menu_session(
    path: String,
    screen_x: i32,
    screen_y: i32,
    app: &tauri::AppHandle,
    ready_tx: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
    let _com_guard = ComGuard::new()?;
    let owner_hwnd = create_owner_window()?;

    // Single active session: if another menu is still open, cancel it now.
    // Posting WM_CANCELMODE to the previous session's owner window makes its
    // TrackPopupMenu modal loop dismiss the old menu immediately.
    let prev_hwnd = ACTIVE_OWNER_HWND.swap(owner_hwnd.0 as usize, Ordering::SeqCst);
    if prev_hwnd != 0 {
        unsafe {
            let _ = PostMessageW(HWND(prev_hwnd as _), WM_CANCELMODE, WPARAM(0), LPARAM(0));
        }
    }

    // 用 Option 包装 ready_tx：show_menu_owned 成功时 take() 并发送 Ok(())，
    // 若 show_menu_owned 早期失败（未发送），则在此处发送错误。
    // oneshot::Sender 不支持 clone，必须用 take() 模式。
    let mut ready_tx_opt = Some(ready_tx);
    let result = show_menu_owned(owner_hwnd, path, screen_x, screen_y, app, &mut ready_tx_opt);
    // 如果 show_menu_owned 未发送 ready_tx（早期失败），在此发送错误
    if let Some(tx) = ready_tx_opt.take() {
        let _ = tx.send(result.clone());
    }

    // Release ownership only if it's still ours (a newer session may have taken over).
    let _ = ACTIVE_OWNER_HWND.compare_exchange(
        owner_hwnd.0 as usize,
        0,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    // Reset the lifecycle only if we're still the current session. Never clears
    // a Hung state (that would re-enable the hung Shell path).
    let _ = SESSION_STATE.compare_exchange(
        SessionState::Tracking as i32,
        SessionState::Idle as i32,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );

    unsafe {
        let _ = DestroyWindow(owner_hwnd);
    }
    result
}

/// Create the hidden owner window that all menu tracking on this thread must use.
fn create_owner_window() -> std::result::Result<HWND, String> {
    // The menu's owner window must be owned by this thread for TrackPopupMenu to
    // display the menu. A hidden "STATIC" control window is a valid owner and
    // needs no custom window class. All Shell/COM work then happens on this
    // thread, fully isolated from the main thread.
    let owner_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    };
    if owner_hwnd.0 == 0 {
        return Err(format!(
            "Failed to create menu owner window, error: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(owner_hwnd)
}

fn show_menu_owned(
    owner_hwnd: HWND,
    path: String,
    screen_x: i32,
    screen_y: i32,
    app: &tauri::AppHandle,
    ready_tx: &mut Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
) -> std::result::Result<(), String> {
    log::info!(
        "Showing context menu for '{}' at ({}, {})",
        path,
        screen_x,
        screen_y
    );

    if !Path::new(&path).exists() {
        return Err(format!("Path does not exist: {}", path));
    }

    // ---- Shell object setup ----
    let desktop: IShellFolder = unsafe {
        SHGetDesktopFolder().map_err(|e| format!("Failed to get desktop folder: {}", e))?
    };

    let parent_path = Path::new(&path)
        .parent()
        .unwrap_or(Path::new(&path))
        .to_string_lossy()
        .to_string();
    let parent_wide = to_wide(&parent_path);

    let mut parent_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        desktop
            .ParseDisplayName(
                None,
                None,
                PCWSTR(parent_wide.as_ptr()),
                None,
                &mut parent_pidl,
                std::ptr::null_mut(),
            )
            .map_err(|e| format!("Failed to parse parent path: {}", e))?
    };

    let parent_folder: IShellFolder = unsafe {
        desktop.BindToObject(parent_pidl, None).map_err(|e| {
            CoTaskMemFree(Some(parent_pidl as *const _));
            format!("Failed to bind parent: {}", e)
        })?
    };

    let file_name = Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            unsafe {
                CoTaskMemFree(Some(parent_pidl as *const _));
            }
            format!("Invalid file name: {}", path)
        })?;
    let file_wide = to_wide(file_name);

    let mut child_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        parent_folder
            .ParseDisplayName(
                None,
                None,
                PCWSTR(file_wide.as_ptr()),
                None,
                &mut child_pidl,
                std::ptr::null_mut(),
            )
            .map_err(|e| {
                CoTaskMemFree(Some(parent_pidl as *const _));
                format!("Failed to parse file name: {}", e)
            })?
    };

    let context_menu: IContextMenu = unsafe {
        parent_folder
            .GetUIObjectOf(None, &[child_pidl], None)
            .map_err(|e| {
                CoTaskMemFree(Some(parent_pidl as *const _));
                CoTaskMemFree(Some(child_pidl as *const _));
                format!("Failed to get IContextMenu: {}", e)
            })?
    };

    // Free PIDLs — shell copied them, we don't need them anymore
    unsafe {
        CoTaskMemFree(Some(parent_pidl as *const _));
        CoTaskMemFree(Some(child_pidl as *const _));
    }

    let context_menu_2: IContextMenu2 = context_menu
        .cast()
        .map_err(|e| format!("Failed to get IContextMenu2: {}", e))?;

    log::info!("[CTX_MENU] Shell objects ready, building popup menu");

    let hmenu =
        unsafe { CreatePopupMenu().map_err(|e| format!("Failed to create popup menu: {}", e))? };

    unsafe {
        context_menu_2
            .QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL)
            .map_err(|e| format!("Failed to query context menu: {}", e))?
    };
    log::info!("[CTX_MENU] QueryContextMenu done");

    // The menu is built; tell the watchdog we are ready so it doesn't time out
    // and fall back to the built-in menu.
    SESSION_STATE.store(SessionState::Tracking as i32, Ordering::SeqCst);
    // take() 取出 Sender 并发送 Ok(())，表示菜单已构建完成
    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(Ok(()));
    }

    // ---- Subclass the owner window to track menu selection ----
    CTX_MENU_CMD.store(0, Ordering::SeqCst);
    unsafe { CTX_MENU_ICM2 = &context_menu_2 as *const IContextMenu2 as usize };

    unsafe {
        SetWindowSubclass(owner_hwnd, Some(ctx_menu_subclass_proc), 1, 0);
    }

    // TPM_RETURNCMD: returns the selected item ID directly, or 0 if the menu was
    // cancelled (user clicked outside). Without this flag, TrackPopupMenu always
    // returns nonzero on success, making it impossible to distinguish "item selected"
    // from "menu dismissed".
    log::info!("[CTX_MENU] TrackPopupMenu starting (main thread stays responsive)");
    let cmd_id = unsafe {
        // SetForegroundWindow 必须调用在 TrackPopupMenu 的 owner 窗口上，
        // 否则点击菜单外部时菜单不会自动关闭
        SetForegroundWindow(owner_hwnd);
        let ret = TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD,
            screen_x,
            screen_y,
            0,
            owner_hwnd,
            None,
        );
        let _ = PostMessageW(owner_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        ret.0 as i32
    };
    log::info!("[CTX_MENU] TrackPopupMenu returned");

    unsafe {
        RemoveWindowSubclass(owner_hwnd, Some(ctx_menu_subclass_proc), 1);
        CTX_MENU_ICM2 = 0;
    }

    log::info!("Context menu command selected: {}", cmd_id);

    if cmd_id > 0 {
        // QueryContextMenu used idCmdFirst=1, so command IDs start at 1.
        // InvokeCommand with high word=0 treats low word as OFFSET (0-based).
        // offset = absolute_id - idCmdFirst = cmd_id - 1
        let offset = (cmd_id - 1) as u32;

        log::info!(
            "[CTX_MENU] Invoking command: absolute_id={}, offset={}",
            cmd_id,
            offset
        );

        // Resolve the canonical verb so we can special-case "properties":
        // invoking it through the context menu object is unreliable in this
        // host context, but SHObjectProperties opens it directly.
        let mut verb_buf = [0u16; 260];
        let verb = if unsafe {
            context_menu_2.GetCommandString(
                offset as usize,           // windows 0.52 需要 usize
                GCS_VERBW,
                None,
                PSTR(verb_buf.as_mut_ptr() as *mut u8),  // 需要 PSTR 而非 *mut i8
                verb_buf.len() as u32,
            )
        }
        .is_ok()
        {
            let end = verb_buf.iter().position(|&c| c == 0).unwrap_or(verb_buf.len());
            Some(String::from_utf16_lossy(&verb_buf[..end]))
        } else {
            None
        };
        log::info!(
            "[CTX_MENU] command {} verb: {:?}",
            cmd_id,
            verb.as_deref().unwrap_or("<unknown>")
        );

        if verb.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("properties")) {
            // 使用 ShellExecuteExW + "properties" verb 打开属性对话框。
            // 这是 Windows 资源管理器自身使用的方法，比 SHObjectProperties 更可靠——
            // 后者在非 Explorer 宿主进程中常返回失败并提示"此项目的属性未知"。
            if !show_properties_via_shell_execute(owner_hwnd, &path) {
                log::warn!(
                    "[CTX_MENU] ShellExecuteExW properties failed for '{}', error: {}",
                    path,
                    std::io::Error::last_os_error()
                );
            }
            log::info!("Context menu command {} (properties) opened", cmd_id);
        } else if verb.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("delete")) {
            // 拦截删除操作：Shell 原生 InvokeCommand 的删除是否显示确认对话框
            // 取决于系统回收站设置，不可控。统一改用 delete_file 函数，确保始终
            // 显示标准删除确认对话框，防止误删除。
            delete_file(owner_hwnd, &path);
            log::info!(
                "Context menu command {} (delete) executed via fallback with confirmation",
                cmd_id
            );
        } else {
            let mut invoke_info: CMINVOKECOMMANDINFOEX = unsafe { std::mem::zeroed() };
            invoke_info.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32;
            invoke_info.fMask = CMIC_MASK_UNICODE;
            // The command's modal dialogs must be parented to a window owned by
            // the calling thread; the main window belongs to the main thread, and a
            // cross-thread owner breaks the shell property-sheet host.
            invoke_info.hwnd = owner_hwnd;
            invoke_info.lpVerb = PCSTR(offset as usize as *const u8);
            invoke_info.lpVerbW = PCWSTR(offset as usize as *const u16);
            invoke_info.nShow = SW_SHOWDEFAULT.0;

            unsafe {
                context_menu_2
                    .InvokeCommand(std::ptr::addr_of!(invoke_info) as *const _)
                    .map_err(|e| format!("Failed to invoke command: {}", e))?
            };
            log::info!("Context menu command {} executed successfully", cmd_id);
        }
    }

    unsafe {
        let _ = DestroyMenu(hmenu);
    }

    // 检查文件是否被删除（如"删除"命令），如果是则立即更新索引
    if !Path::new(&path).exists() {
        log::info!(
            "File '{}' no longer exists after context menu action, updating index",
            path
        );
        let vm = app.state::<AppState>().volume_manager.clone();
        let path_clone = path.clone();
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            let mut vm = vm.lock().await;
            vm.remove_file(&path_clone);
            drop(vm);
            let _ = app_clone.emit(
                "index-updated",
                serde_json::json!({
                    "volume": "",
                    "added": 0,
                    "updated": 0,
                    "removed": 1,
                    "total": 0,
                    "cache_total": 0
                }),
            );
        });
    }

    // 每次右键菜单操作后都通知前端刷新当前可见范围。
    // 覆盖所有场景：
    //   - 删除：文件已不存在，前端重新获取后自然移除
    //   - 剪切+粘贴：粘贴发生在另一个 show_context_menu 调用中，
    //     该调用的 path 是目标文件夹（仍存在），但源文件已被移走，
    //     刷新后前端重新获取数据，被移走的文件不再出现
    //   - 其他操作（打开/属性等）：无害的轻微开销
    {
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            // 使用独立事件名，避免被 records-refresh 的空变化过滤拦截
            let _ = app_clone.emit("refresh-visible", ());
        });
    }

    Ok(())
}

/// Window subclass — tracks WM_MENUSELECT to remember which item is highlighted,
/// and handles IContextMenu2 messages for submenus/owner-draw.
unsafe extern "system" fn ctx_menu_subclass_proc(
    hwnd: HWND,
    umsg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match umsg {
        // WM_MENUSELECT: low word of wparam = menu item ID (or 0xFFFF for separator/submenu close)
        // High word = flags (MF_*, MF_POPUP if submenu)
        WM_MENUSELECT => {
            let raw_id = wparam.0 & 0xFFFF;
            let flags = (wparam.0 >> 16) & 0xFFFF;

            // If this is a top-level item (not a submenu) and not a separator/close
            if (flags & MF_POPUP.0 as usize) == 0 && raw_id != 0xFFFF && raw_id <= 0x7FFF {
                log::info!(
                    "[CTX_MENU] WM_MENUSELECT: id={}, flags=0x{:X}",
                    raw_id,
                    flags
                );
                CTX_MENU_CMD.store(raw_id as i32, Ordering::SeqCst);
            }
        }
        // WM_COMMAND from the menu — direct selection (some items do send this)
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            log::info!("[CTX_MENU] WM_COMMAND: id={}", id);
            if (1..=0x7FFF).contains(&id) {
                CTX_MENU_CMD.store(id, Ordering::SeqCst);
            }
            return LRESULT(0);
        }
        // Forward owner-draw and submenu messages to IContextMenu2
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR => {
            let ptr = CTX_MENU_ICM2;
            if ptr != 0 {
                let icm2 = &*(ptr as *const IContextMenu2);
                let _ = icm2.HandleMenuMsg(umsg, wparam, lparam);
                if umsg == WM_MENUCHAR {
                    return LRESULT(0);
                }
                return LRESULT(0);
            }
        }
        _ => {}
    }
    DefSubclassProc(hwnd, umsg, wparam, lparam)
}

struct ComGuard;

impl ComGuard {
    fn new() -> std::result::Result<Self, String> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .map_err(|e| format!("CoInitializeEx failed: {}", e))?;
        }
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Built-in fallback menu
//
// Used when the Shell context menu cannot be built (the hung-shell-component
// behaviour observed after sleep/resume stalls QueryContextMenu forever). Only
// basic operations are provided, implemented with plain Win32 so they never
// touch the broken Shell extensions.
// ---------------------------------------------------------------------------

const FALLBACK_OPEN: usize = 1;
const FALLBACK_OPEN_LOCATION: usize = 2;
const FALLBACK_COPY_PATH: usize = 3;
const FALLBACK_DELETE: usize = 4;
const FALLBACK_PROPERTIES: usize = 5;

async fn show_fallback_menu_async(
    path: String,
    screen_x: i32,
    screen_y: i32,
    app: tauri::AppHandle,
) -> std::result::Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = show_fallback_menu(path, screen_x, screen_y, &app);
        let _ = tx.send(result);
    });
    rx.await
        .map_err(|e| format!("Fallback menu task failed: {}", e))?
}

fn show_fallback_menu(
    path: String,
    screen_x: i32,
    screen_y: i32,
    app: &tauri::AppHandle,
) -> std::result::Result<(), String> {
    let _com_guard = ComGuard::new()?;
    let owner_hwnd = create_owner_window()?;

    log::info!(
        "[CTX_MENU] Showing built-in fallback menu for '{}' at ({}, {})",
        path,
        screen_x,
        screen_y
    );

    let hmenu = unsafe {
        CreatePopupMenu().map_err(|e| format!("Failed to create popup menu: {}", e))?
    };
    unsafe {
        let _ = AppendMenuW(hmenu, MF_STRING, FALLBACK_OPEN, w!("打开(&O)"));
        let _ = AppendMenuW(hmenu, MF_STRING, FALLBACK_OPEN_LOCATION, w!("打开所在位置(&I)"));
        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(hmenu, MF_STRING, FALLBACK_COPY_PATH, w!("复制路径(&C)"));
        let _ = AppendMenuW(hmenu, MF_STRING, FALLBACK_DELETE, w!("删除(&D)"));
        let _ = AppendMenuW(hmenu, MF_STRING, FALLBACK_PROPERTIES, w!("属性(&R)"));
    }

    // 超时保护：启动定时器线程，FALLBACK_MENU_TIMEOUT_SECS 秒后若菜单仍未关闭，
    // 向 owner 窗口发送 WM_CANCELMODE 强制取消 TrackPopupMenu。
    // 防止休眠/唤醒后 TrackPopupMenu 永不返回导致前端 invoke 无响应。
    let menu_done = Arc::new(AtomicBool::new(false));
    let menu_done_clone = menu_done.clone();
    let owner_hwnd_for_timer = owner_hwnd;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(FALLBACK_MENU_TIMEOUT_SECS));
        if !menu_done_clone.load(Ordering::SeqCst) {
            log::warn!(
                "[CTX_MENU] Fallback menu timed out after {}s, cancelling",
                FALLBACK_MENU_TIMEOUT_SECS
            );
            unsafe {
                let _ = PostMessageW(owner_hwnd_for_timer, WM_CANCELMODE, WPARAM(0), LPARAM(0));
            }
        }
    });

    let cmd_id = unsafe {
        // SetForegroundWindow 必须调用在 TrackPopupMenu 的 owner 窗口上，
        // 否则点击菜单外部时菜单不会自动关闭
        SetForegroundWindow(owner_hwnd);
        let ret = TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD,
            screen_x,
            screen_y,
            0,
            owner_hwnd,
            None,
        );
        let _ = PostMessageW(owner_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        ret.0 as usize
    };
    // 标记菜单已返回，定时器线程不再需要发送 WM_CANCELMODE
    menu_done.store(true, Ordering::SeqCst);
    log::info!("[CTX_MENU] Fallback menu command selected: {}", cmd_id);

    unsafe {
        let _ = DestroyMenu(hmenu);
    }

    match cmd_id {
        FALLBACK_OPEN => shell_open(owner_hwnd, &path),
        FALLBACK_OPEN_LOCATION => open_location(&path),
        FALLBACK_COPY_PATH => copy_path_to_clipboard(owner_hwnd, &path),
        FALLBACK_DELETE => delete_file(owner_hwnd, &path),
        FALLBACK_PROPERTIES => show_properties_dialog(owner_hwnd, &path),
        _ => {}
    }

    unsafe {
        let _ = DestroyWindow(owner_hwnd);
    }

    // 每次右键菜单操作后都通知前端刷新当前可见范围。
    {
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = app_clone.emit("refresh-visible", ());
        });
    }

    Ok(())
}

fn shell_open(owner_hwnd: HWND, path: &str) {
    let path_wide = to_wide(path);
    unsafe {
        // windows 0.52 版本冲突导致 Option<T> 不实现 IntoParam，
        // 直接传值而非 Some/None 来绕过
        let _ = ShellExecuteW(
            owner_hwnd,
            w!("open"),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn open_location(path: &str) {
    // explorer.exe /select,"<path>"
    let args = format!("/select,\"{}\"", path);
    let args_wide = to_wide(&args);
    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            w!("open"),
            w!("explorer.exe"),
            PCWSTR(args_wide.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn copy_path_to_clipboard(owner_hwnd: HWND, path: &str) {
    let wide = to_wide(path);
    // windows 0.52 中 GlobalAlloc 返回 Result<HGLOBAL>
    let h_mem = match unsafe { GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) } {
        Ok(h) => h,
        Err(_) => return,
    };
    if h_mem.0.is_null() {
        return;
    }
    unsafe {
        // GlobalLock 在 0.52 中返回 *mut c_void
        let dst = GlobalLock(h_mem) as *mut u16;
        if !dst.is_null() {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(h_mem);
        }
    }
    // OpenClipboard 直接传 HWND，不用 Option 绕过版本冲突
    if unsafe { OpenClipboard(owner_hwnd).is_err() } {
        unsafe {
            let _ = GlobalFree(h_mem);
        }
        return;
    }
    unsafe {
        let _ = EmptyClipboard();
        // HANDLE 字段是 isize，需要从 *mut c_void 转换
        let h = SetClipboardData(13u32, HANDLE(h_mem.0 as isize)); // CF_UNICODETEXT = 13
        let _ = CloseClipboard();
        if h.is_err() {
            let _ = GlobalFree(h_mem);
        }
    }
}

fn delete_file(owner_hwnd: HWND, path: &str) {
    // 先用 MessageBoxW 显示自定义确认对话框，确保对话框显示在最前面。
    // 不依赖 SHFileOperationW 的内置确认，因为后者以隐藏的 owner_hwnd 为父窗口，
    // 对话框可能显示在后台，用户看不到，导致误以为窗口空白。
    let msg = format!("您确定要将此文件移到回收站吗？\n\n{}", path);
    let msg_wide = to_wide(&msg);
    let title_wide = to_wide("确认删除");
    let choice = unsafe {
        MessageBoxW(
            owner_hwnd,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    if choice != IDYES {
        log::info!("[CTX_MENU] Delete cancelled by user");
        return;
    }

    // 用户确认后执行删除，使用 FOF_NOCONFIRMATION 跳过系统内置确认
    let from = path.encode_utf16().chain([0u16, 0u16]).collect::<Vec<u16>>();
    let mut ops: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    ops.hwnd = owner_hwnd;
    ops.wFunc = FO_DELETE;
    ops.pFrom = PCWSTR(from.as_ptr());
    // FOF_ALLOWUNDO: 允许撤销（删除到回收站）
    // FOF_NOCONFIRMATION: 跳过系统确认对话框（已用 MessageBoxW 确认过）
    ops.fFlags = (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0) as u16;
    // SHFileOperationW 需要 *mut SHFILEOPSTRUCTW
    let result = unsafe { SHFileOperationW(&mut ops) };
    log::info!("[CTX_MENU] Fallback delete result: {}", result);
}

fn show_properties_dialog(owner_hwnd: HWND, path: &str) {
    // 同样使用 ShellExecuteExW + "properties" verb，与主菜单保持一致
    if !show_properties_via_shell_execute(owner_hwnd, path) {
        log::warn!(
            "[CTX_MENU] Fallback properties failed for '{}', error: {}",
            path,
            std::io::Error::last_os_error()
        );
    }
}

/// 使用 ShellExecuteExW + "properties" verb 打开文件属性对话框。
/// 这是 Windows 资源管理器自身使用的方法，比 SHObjectProperties 更可靠。
/// SHObjectProperties 在非 Explorer 宿主进程中常返回失败并提示"此项目的属性未知"。
/// SEE_MASK_INVOKEIDLIST 确保 verb 通过 IContextMenu::InvokeCommand 路由，
/// 而不是简单的 ShellExecute，从而正确显示属性页。
fn show_properties_via_shell_execute(owner_hwnd: HWND, path: &str) -> bool {
    let path_wide = to_wide(path);
    let verb_wide = to_wide("properties");
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_INVOKEIDLIST;
    info.hwnd = owner_hwnd;
    info.lpVerb = PCWSTR(verb_wide.as_ptr());
    info.lpFile = PCWSTR(path_wide.as_ptr());
    info.nShow = SW_SHOW.0;
    // windows 0.52 中 ShellExecuteExW 返回 Result<()>，用 is_ok() 判断成功
    unsafe { ShellExecuteExW(&mut info).is_ok() }
}
