// fs module - Windows file system operations
// mft and usn functionality moved to index/monitor.rs for simplified implementation

use crate::error::{AppError, Result};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
};

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub drive_letter: String,
    pub volume_name: String,
    pub file_system: String,
    pub total_size: u64,
    pub free_space: u64,
}

/// 获取所有可用卷（不限文件系统类型）
pub fn get_all_volumes() -> Result<Vec<VolumeInfo>> {
    let mut volumes = Vec::new();

    let drives = unsafe { GetLogicalDrives() };
    if drives == 0 {
        return Err(AppError::WindowsApi(
            windows::core::Error::from_win32().code().0 as u32,
        ));
    }

    for i in 0..26 {
        if (drives & (1 << i)) != 0 {
            let drive_letter = format!("{}:", (b'A' + i as u8) as char);

            if let Ok(info) = get_volume_info(&drive_letter) {
                volumes.push(info);
            }
        }
    }

    Ok(volumes)
}

pub fn get_volume_info(drive_letter: &str) -> Result<VolumeInfo> {
    let root_path = format!("{}\\", drive_letter);
    let root_path_wide: Vec<u16> = root_path.encode_utf16().chain(std::iter::once(0)).collect();

    let mut volume_name_buffer = [0u16; 256];
    let mut file_system_buffer = [0u16; 256];
    let mut serial_number: u32 = 0;
    let mut max_component_length: u32 = 0;
    let mut file_system_flags: u32 = 0;

    let success = unsafe {
        GetVolumeInformationW(
            windows::core::PCWSTR(root_path_wide.as_ptr()),
            Some(&mut volume_name_buffer),
            Some(&mut serial_number),
            Some(&mut max_component_length),
            Some(&mut file_system_flags),
            Some(&mut file_system_buffer),
        )
    };

    if let Err(e) = success {
        return Err(AppError::WindowsApi(e.code().0 as u32));
    }

    let mut free_bytes_available: u64 = 0;
    let mut total_number_of_bytes: u64 = 0;
    let mut total_number_of_free_bytes: u64 = 0;

    let success = unsafe {
        GetDiskFreeSpaceExW(
            windows::core::PCWSTR(root_path_wide.as_ptr()),
            Some(&mut free_bytes_available),
            Some(&mut total_number_of_bytes),
            Some(&mut total_number_of_free_bytes),
        )
    };

    if let Err(e) = success {
        return Err(AppError::WindowsApi(e.code().0 as u32));
    }

    let volume_name = String::from_utf16_lossy(
        &volume_name_buffer[..volume_name_buffer.iter().position(|&c| c == 0).unwrap_or(0)],
    );
    let file_system = String::from_utf16_lossy(
        &file_system_buffer[..file_system_buffer.iter().position(|&c| c == 0).unwrap_or(0)],
    );

    Ok(VolumeInfo {
        drive_letter: drive_letter.to_string(),
        volume_name,
        file_system,
        total_size: total_number_of_bytes,
        free_space: free_bytes_available,
    })
}

pub fn is_elevated() -> bool {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    let volume_path = "\\\\.\\C:\0";
    let volume_path_wide: Vec<u16> = volume_path.encode_utf16().collect();

    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(volume_path_wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };

    match handle {
        Ok(handle) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            true
        }
        Err(_) => false,
    }
}

/// 以管理员权限重新启动当前进程（UAC 提权）。
///
/// 通过 `ShellExecuteW` + `runas` verb 弹出 UAC 确认框；确认后派生当前程序的
/// 高权限副本（附带原始参数 + `--elevated`），原进程应立即退出。
/// 返回 `Err(code)` 表示用户取消（SE_ERR_ACCESSDENIED=5）或系统不支持提权。
pub fn relaunch_elevated() -> std::result::Result<(), i32> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe().map_err(|_| 0)?;
    let exe_str = exe.to_string_lossy();

    // 拼接原始参数（跳过 args[0] 程序名）+ "--elevated"
    let mut params = String::new();
    for (i, arg) in std::env::args().skip(1).enumerate() {
        if i > 0 {
            params.push(' ');
        }
        params.push_str(&arg);
    }
    if !params.is_empty() {
        params.push(' ');
    }
    params.push_str("--elevated");

    let verb = "runas\0";
    let verb_wide: Vec<u16> = verb.encode_utf16().collect();
    let file_wide: Vec<u16> = exe_str.encode_utf16().chain(std::iter::once(0)).collect();
    let params_wide: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();

    let result = unsafe {
        ShellExecuteW(
            None,
            windows::core::PCWSTR(verb_wide.as_ptr()),
            windows::core::PCWSTR(file_wide.as_ptr()),
            windows::core::PCWSTR(params_wide.as_ptr()),
            windows::core::PCWSTR(std::ptr::null()),
            SW_SHOWNORMAL,
        )
    };

    // 返回值 <= 32 表示失败（如 SE_ERR_ACCESSDENIED=5 用户取消）
    let code = result.0 as i32;
    if code > 32 {
        Ok(())
    } else {
        Err(code)
    }
}
