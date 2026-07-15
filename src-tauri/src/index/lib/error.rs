use std::fmt;

#[derive(Debug)]
pub enum MftError {
    Io(std::io::Error),
    NtfsParse(String),
    NtfsPanic(String),
    PermissionDenied { path: String },
}

impl fmt::Display for MftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MftError::Io(e) => write!(f, "I/O error: {}", e),
            MftError::NtfsParse(msg) => write!(f, "NTFS parse error: {}", msg),
            MftError::NtfsPanic(msg) => write!(f, "NTFS parser panicked: {}", msg),
            MftError::PermissionDenied { path } => {
                write!(
                    f,
                    "Permission denied opening '{}'. \
                     On Windows, raw volume access (\\\\.\\C:) requires Administrator privileges. \
                     Run as Administrator or provide an NTFS image file path.",
                    path
                )
            }
        }
    }
}

impl std::error::Error for MftError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MftError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MftError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            MftError::PermissionDenied {
                path: String::new(),
            }
        } else {
            MftError::Io(e)
        }
    }
}
