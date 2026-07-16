// 集成 Windows Shell 原生右键菜单
//
// 功能：
// - 通过 SHCreateItemFromParsingName 从路径创建 IShellItem
// - 用 SHCreateDefaultContextMenu + DEFCONTEXTMENU 构造 IContextMenu3
//   （原方案用 IShellItem::BindToHandler(BHID_ContextMenu) 错误地试图直接从
//   单个 IShellItem 拿 IContextMenu，导致 MK_E_UNAVAILABLE 错误。
//   IContextMenu 必须由父目录的 IShellFolder 通过 IShellFolder::GetUIObjectOf
//   或 SHCreateDefaultContextMenu 创建。这是 Windows Shell 的标准做法。）
// - TrackPopupMenuEx 弹出与资源管理器一致的菜单（含第三方扩展）
// - 用户选择命令后调用 IContextMenu3::InvokeCommand 执行
//
// 多线程模型：
// - Tauri command 用 spawn_blocking 跑在独立线程
// - 该线程初始化 COM(STA)，独立消息循环
//
// 依赖：windows crate 0.52 已开启 Win32_UI_Shell / Win32_System_Com / Win32_UI_WindowsAndMessaging

// 类型定义
use tauri::{AppHandle, Manager, WebviewWindow};
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu3, ILClone, ILFindLastID, ILFree, ILRemoveLastID, IShellFolder, IShellItem,
    SHCreateDefaultContextMenu, SHCreateItemFromParsingName, SHGetIDListFromObject, CMF_NORMAL,
    CMINVOKECOMMANDINFO, DEFCONTEXTMENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, SetForegroundWindow, TrackPopupMenuEx, TPM_LEFTALIGN,
    TPM_RETURNCMD, TPM_TOPALIGN,
};

// 常量
/// BHID_SFObject：用于 IShellItem::BindToHandler 拿父目录的 IShellFolder 接口
/// GUID 值来自 Windows SDK shobjidl.h
/// 命名遵循 Windows 头文件中的 GUID 变量命名（BHID_*），但 Rust 编译器要求常量全大写，
/// 所以命名为 BHID_SFOBJECT。
const BHID_SFOBJECT: windows::core::GUID =
    windows::core::GUID::from_u128(0x3981e224_f559_11d3_8e3a_00c04f6837d5_u128);

// 组件主体

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

        // 5. 拿完整绝对 PIDL（包含从桌面根到子项的整条 ID 链）
        //    SHGetIDListFromObject 接受任何 IUnknown，返回 IShellItem 对应的 PIDL
        //    诊断日志：PIDL 分配失败通常意味着内存不足（罕见）
        log::info!("[CTX_MENU] Getting full PIDL from IShellItem");
        let full_pidl: *mut ITEMIDLIST = unsafe {
            SHGetIDListFromObject(&item).map_err(|e| {
                log::error!("[CTX_MENU] SHGetIDListFromObject failed: {}", e);
                format!("SHGetIDListFromObject failed: {}", e)
            })?
        };
        if full_pidl.is_null() {
            return Err("SHGetIDListFromObject returned null PIDL".to_string());
        }

        // 6. 拆分 PIDL：父目录 PIDL + 子项 PIDL
        //    Windows PIDL 格式：每段 SHITEMID(cb + abID)，最后一段 cb=0 表示终止。
        //    完整 PIDL = 段1 + 段2 + ... + 段N + 终止段
        //      - 父目录 PIDL = 去掉最后一段（ILRemoveLastID 原地修改）
        //      - 子项 PIDL = 最后一段（ILFindLastID 指向完整 PIDL 内部，不独立分配）
        //    诊断日志：拆分过程
        log::info!("[CTX_MENU] Splitting full PIDL into folder + child PIDL");
        // 复制完整 PIDL，然后 in-place 移除最后一段，得到父目录 PIDL
        let folder_pidl: *mut ITEMIDLIST = unsafe { ILClone(full_pidl) };
        if folder_pidl.is_null() {
            // 完整 PIDL 由调用者负责释放，folder_pidl 失败就直接 return；
            // 完整 PIDL 在函数末尾统一释放。
            unsafe {
                ILFree(Some(full_pidl as *const _));
            }
            return Err("ILClone returned null for folder PIDL".to_string());
        }
        unsafe {
            ILRemoveLastID(Some(folder_pidl));
        }
        // 子项 PIDL 必须独立分配，否则 ILFree 完整 PIDL 后该指针悬空
        let child_pidl_source: *mut ITEMIDLIST = unsafe { ILFindLastID(full_pidl) };
        let child_pidl: *mut ITEMIDLIST = unsafe { ILClone(child_pidl_source) };
        if child_pidl.is_null() {
            unsafe {
                ILFree(Some(full_pidl as *const _));
            }
            unsafe {
                ILFree(Some(folder_pidl as *const _));
            }
            return Err("ILClone returned null for child PIDL".to_string());
        }
        log::info!(
            "[CTX_MENU] PIDL split done: folder_pidl={:p}, child_pidl={:p}",
            folder_pidl,
            child_pidl
        );

        // 7. 拿父目录的 IShellFolder
        //    流程：item.GetParent() → 父目录 IShellItem → IShellItem::BindToHandler(BHID_SFObject)
        //    BindToHandler 在这里对父目录的 IShellItem 调用是合法的（与对子项的非法调用不同），
        //    因为父目录的 IShellItem 的 BindToHandler 内部能正确解析为父目录的 IShellFolder。
        //    诊断日志：每一步独立记录
        log::info!("[CTX_MENU] Getting parent IShellItem");
        let parent_item: IShellItem = unsafe {
            item.GetParent().map_err(|e| {
                log::error!("[CTX_MENU] IShellItem::GetParent failed: {}", e);
                format!("IShellItem::GetParent failed: {}", e)
            })?
        };
        log::info!("[CTX_MENU] Calling BindToHandler(BHID_SFObject) on parent IShellItem");
        let parent_folder: IShellFolder = unsafe {
            parent_item
                .BindToHandler(None, &BHID_SFOBJECT)
                .map_err(|e| {
                    log::error!(
                        "[CTX_MENU] IShellItem::BindToHandler(BHID_SFObject) failed: {}",
                        e
                    );
                    format!("IShellItem::BindToHandler(BHID_SFObject) failed: {}", e)
                })?
        };
        log::info!("[CTX_MENU] Got parent IShellFolder");

        // 8. 构造 DEFCONTEXTMENU
        //    - hwnd: 父窗口（TrackPopupMenuEx 也用它）
        //    - pcmcb: 回调（None）
        //    - pidlFolder: 父目录绝对 PIDL（DEFCONTEXTMENU 用来解析父目录路径/属性）
        //    - psf: 父目录 IShellFolder（DEFCONTEXTMENU 用来枚举子项扩展）
        //    - cidl: 子项数量（1）
        //    - apidl: 子项 PIDL 数组（*mut *mut ITEMIDLIST，1 个元素的栈数组指针）
        //    - punkAssociationInfo: 关联信息（None，让 DEFCONTEXTMENU 内部解析）
        //    - cKeys/aKeys: 注册表键（0 / null）
        //    诊断日志：结构体构造前
        log::info!("[CTX_MENU] Building DEFCONTEXTMENU structure");
        // apidl 必须是 *mut *mut ITEMIDLIST 类型（指向 PIDL 指针的数组）
        // 用栈上 [child_pidl; 1] 数组的 .as_mut_ptr()
        // 注意：apidl_array 不能在 dcm 复制/读取前 drop
        let mut apidl_array: [*mut ITEMIDLIST; 1] = [child_pidl];
        let dcm = DEFCONTEXTMENU {
            hwnd,
            pcmcb: std::mem::ManuallyDrop::new(None),
            pidlFolder: folder_pidl,
            psf: std::mem::ManuallyDrop::new(Some(parent_folder)),
            cidl: 1,
            apidl: apidl_array.as_mut_ptr(),
            punkAssociationInfo: std::mem::ManuallyDrop::new(None),
            cKeys: 0,
            aKeys: std::ptr::null(),
        };
        log::info!("[CTX_MENU] DEFCONTEXTMENU structure built");

        // 9. 调 SHCreateDefaultContextMenu 拿 IContextMenu3
        //    这是 Windows 7+ 官方 API：内部会枚举 pidlFolder + apidl 对应的所有
        //    shell 扩展（verbs），并合并到返回的 IContextMenu 实例中。
        //    windows 0.52 实际签名是 `fn(pdcm: *const DEFCONTEXTMENU) -> Result<T>`，
        //    不需要 IObjectArray（任务中给的方案 A/B 实际是基于更高版本 windows crate 的伪代码）。
        log::info!("[CTX_MENU] Calling SHCreateDefaultContextMenu");
        let ctx_menu: IContextMenu3 = unsafe {
            SHCreateDefaultContextMenu(&dcm).map_err(|e| {
                log::error!("[CTX_MENU] SHCreateDefaultContextMenu failed: {}", e);
                format!("SHCreateDefaultContextMenu failed: {}", e)
            })?
        };
        log::info!("[CTX_MENU] Got IContextMenu3 via SHCreateDefaultContextMenu");

        // 10. DEFCONTEXTMENU 已被 SHCreateDefaultContextMenu 内部读完，可以释放资源：
        //     - apidl_array 是栈数组（Copy 类型），显式 let _ = 防止编译器优化时提前 drop
        //     - folder_pidl / full_pidl / child_pidl 都是独立分配的 PIDL，需 ILFree
        //     诊断日志：释放前
        log::info!("[CTX_MENU] Releasing PIDL resources");
        let _ = apidl_array;
        unsafe {
            ILFree(Some(full_pidl as *const _));
        }
        unsafe {
            ILFree(Some(folder_pidl as *const _));
        }
        unsafe {
            ILFree(Some(child_pidl as *const _));
        }
        log::info!("[CTX_MENU] PIDL resources released");

        // 11. 创建弹出菜单
        // windows 0.52 中 CreatePopupMenu 返回 Result<HMENU, Error>，用 ? 直接解包
        // 诊断日志：记录 HMENU 句柄值，便于追踪
        log::info!("[CTX_MENU] Creating popup menu");
        let menu = unsafe { CreatePopupMenu() }.map_err(|e| {
            log::error!("[CTX_MENU] CreatePopupMenu failed: {}", e);
            format!("CreatePopupMenu failed: {}", e)
        })?;
        log::info!("[CTX_MENU] Popup menu created: HMENU={:?}", menu);

        // 12. 填充菜单项
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

        // 13. 弹出菜单（设置前置窗口确保 Z-order 正确）
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

        // 14. 销毁菜单句柄
        // 诊断日志：菜单销毁前的最终记录点
        unsafe {
            DestroyMenu(menu).ok();
        }
        log::info!("[CTX_MENU] Menu destroyed");

        // 15. 用户选中某项则执行命令
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
            //（IContextMenu3 内部会与 idFirst 相减得到 verb 偏移）。
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

    // 16. 反初始化 COM
    // 诊断日志：COM 释放前的最终状态记录（result 是 Ok/Err）
    log::info!(
        "[CTX_MENU] About to CoUninitialize, inner result: {:?}",
        result.as_ref().map(|_| "ok").map_err(|e| e.as_str())
    );
    unsafe {
        CoUninitialize();
    }
    result
}
