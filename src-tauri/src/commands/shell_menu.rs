use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use tauri::Emitter;
use tauri::Manager;
use tauri::State;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::search::AppState;

static CTX_MENU_CMD: AtomicI32 = AtomicI32::new(0);
static mut CTX_MENU_ICM2: usize = 0;

/// Show Windows native Shell context menu at specified screen coordinates
#[tauri::command]
pub fn show_context_menu(
    path: String,
    screen_x: i32,
    screen_y: i32,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
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

    let _com_guard = ComGuard::new()?;
    let hwnd = get_main_window_hwnd(&app)?;

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

    let hmenu =
        unsafe { CreatePopupMenu().map_err(|e| format!("Failed to create popup menu: {}", e))? };

    unsafe {
        context_menu_2
            .QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL)
            .map_err(|e| format!("Failed to query context menu: {}", e))?
    };

    // ---- Subclass the Tauri window to track menu selection ----
    CTX_MENU_CMD.store(0, Ordering::SeqCst);
    unsafe { CTX_MENU_ICM2 = &context_menu_2 as *const IContextMenu2 as usize };

    unsafe {
        SetWindowSubclass(hwnd, Some(ctx_menu_subclass_proc), 1, 0);
    }

    unsafe {
        SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(hmenu, TPM_RIGHTBUTTON, screen_x, screen_y, 0, hwnd, None);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    }

    unsafe {
        RemoveWindowSubclass(hwnd, Some(ctx_menu_subclass_proc), 1);
        CTX_MENU_ICM2 = 0;
    }

    let cmd_id = CTX_MENU_CMD.swap(0, Ordering::SeqCst);

    log::info!("Context menu command selected: {}", cmd_id);

    if cmd_id > 0 {
        // QueryContextMenu used idCmdFirst=1, so command IDs start at 1.
        // InvokeCommand with high word=0 treats low word as OFFSET (0-based).
        // offset = absolute_id - idCmdFirst = cmd_id - 1
        let offset = (cmd_id - 1) as u32;
        let verb_ptr = PCSTR(offset as usize as *const u8);

        log::info!(
            "[CTX_MENU] Invoking command: absolute_id={}, offset={}",
            cmd_id,
            offset
        );

        let mut invoke_info: CMINVOKECOMMANDINFO = unsafe { std::mem::zeroed() };
        invoke_info.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32;
        invoke_info.hwnd = hwnd;
        invoke_info.lpVerb = verb_ptr;
        invoke_info.nShow = SW_SHOWDEFAULT.0;

        unsafe {
            context_menu_2
                .InvokeCommand(&invoke_info)
                .map_err(|e| format!("Failed to invoke command: {}", e))?
        };
        log::info!("Context menu command {} executed successfully", cmd_id);
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
        let vm = state.volume_manager.clone();
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

fn get_main_window_hwnd(app: &tauri::AppHandle) -> std::result::Result<HWND, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Failed to get main window".to_string())?;
    let hwnd = HWND(
        window
            .hwnd()
            .map_err(|e| format!("Failed to get HWND: {}", e))?
            .0 as _,
    );
    log::info!("Got main window HWND: {:?}", hwnd);
    Ok(hwnd)
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
