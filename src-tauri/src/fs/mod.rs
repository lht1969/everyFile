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
    #[allow(dead_code)]
    pub device_path: String,
    pub volume_name: String,
    pub file_system: String,
    pub total_size: u64,
    pub free_space: u64,
    #[allow(dead_code)]
    pub serial_number: u32,
}

pub fn get_ntfs_volumes() -> Result<Vec<VolumeInfo>> {
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
                if info.file_system.eq_ignore_ascii_case("NTFS") {
                    volumes.push(info);
                }
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

    if success.is_err() {
        return Err(AppError::WindowsApi(success.unwrap_err().code().0 as u32));
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

    if success.is_err() {
        return Err(AppError::WindowsApi(success.unwrap_err().code().0 as u32));
    }

    let volume_name = String::from_utf16_lossy(
        &volume_name_buffer[..volume_name_buffer.iter().position(|&c| c == 0).unwrap_or(0)],
    );
    let file_system = String::from_utf16_lossy(
        &file_system_buffer[..file_system_buffer.iter().position(|&c| c == 0).unwrap_or(0)],
    );

    Ok(VolumeInfo {
        drive_letter: drive_letter.to_string(),
        device_path: format!("\\\\.\\{}", drive_letter),
        volume_name,
        file_system,
        total_size: total_number_of_bytes,
        free_space: free_bytes_available,
        serial_number,
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
