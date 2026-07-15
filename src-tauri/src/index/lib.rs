pub mod aligned_reader;
pub mod error;
pub mod path;
pub mod scanner;

use std::fs::File;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub use aligned_reader::AlignedReader;
pub use error::MftError;
pub use path::{format_ntfs_time, resolve_paths};
pub use scanner::{format_attributes, MftScanner, ScanOutput, ScanResult};

pub struct NtfsInfo {
    pub cluster_size: u32,
    pub sector_size: u16,
}

pub fn scan_volume(path: &str, max_records: u64) -> Result<(ScanOutput, NtfsInfo), MftError> {
    let (mut reader, ntfs, info) = open_volume(path)?;
    let mut scanner = MftScanner::new(&ntfs, &mut reader)?;
    let output = scanner.scan(&mut reader, max_records);
    Ok((output, info))
}

pub fn open_volume(path: &str) -> Result<(AlignedReader, ntfs::Ntfs, NtfsInfo), MftError> {
    let file = File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            MftError::PermissionDenied {
                path: path.to_string(),
            }
        } else {
            MftError::Io(e)
        }
    })?;

    let mut reader = AlignedReader::new(file, 512)?;

    let mut ntfs = catch_unwind(AssertUnwindSafe(|| ntfs::Ntfs::new(&mut reader)))
        .map_err(|payload| {
            let msg = payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            MftError::NtfsPanic(msg.to_string())
        })
        .and_then(|result| result.map_err(|e| MftError::NtfsParse(e.to_string())))?;

    let _ = ntfs.read_upcase_table(&mut reader);

    let info = NtfsInfo {
        cluster_size: ntfs.cluster_size(),
        sector_size: ntfs.sector_size(),
    };

    Ok((reader, ntfs, info))
}
