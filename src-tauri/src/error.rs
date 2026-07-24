use std::fmt;

#[derive(Debug)]
pub enum AppError {
    WindowsApi(u32),
    Io(std::io::Error),
    Regex(regex::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::WindowsApi(code) => write!(f, "Windows API error: {}", code),
            AppError::Io(e) => write!(f, "IO error: {}", e),
            AppError::Regex(e) => write!(f, "Regex error: {}", e),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e)
    }
}

impl From<regex::Error> for AppError {
    fn from(e: regex::Error) -> Self {
        AppError::Regex(e)
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
