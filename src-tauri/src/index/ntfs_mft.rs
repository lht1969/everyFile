//! File metadata extraction via std::fs::metadata.
//!
//! Returns file size, modified time, and directory flag.
//! Uses only std::fs::metadata (GetFileAttributesExW) instead of
//! FSCTL_READ_FILE_USN_DATA to minimize kernel calls.

use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
};
use windows::core::HSTRING;

/// Open a volume handle for USN operations.
pub(crate) fn open_volume_handle(drive_letter: char) -> Option<HANDLE> {
    let volume_path = HSTRING::from(format!("\\\\.\\{}:", drive_letter));
    let handle = unsafe {
        CreateFileW(
            &volume_path,
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };
    handle.ok()
}

/// Metadata extracted for a single file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileMetadata {
    pub size: u64,
    pub modified_time: i64,
    pub is_directory: bool,
}

/// Simple wrapper for metadata lookups.
pub(crate) struct UsnMetadataReader {
    _marker: (),
}

impl UsnMetadataReader {
    pub fn new(_handle: HANDLE) -> Self {
        Self { _marker: () }
    }

    /// Get file metadata using only std::fs::metadata.
    /// Eliminates the per-file FSCTL_READ_FILE_USN_DATA kernel call.
    ///
    /// `fid` parameter is kept for API compatibility but not used.
    #[inline]
    pub fn get_file_metadata(
        &mut self,
        _fid: u64,
        path: &std::path::Path,
    ) -> FileMetadata {
        match std::fs::metadata(path) {
            Ok(m) => {
                let modified_time = m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                FileMetadata {
                    size: m.len(),
                    modified_time,
                    is_directory: m.is_dir(),
                }
            }
            Err(_) => FileMetadata {
                size: 0,
                modified_time: 0,
                is_directory: false,
            },
        }
    }
}
