use std::fmt;

#[derive(Debug)]
pub enum AppError {
    WindowsApi(u32),
    Io(std::io::Error),
    Database(rusqlite::Error),
    Regex(regex::Error),
    InvalidInput(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::WindowsApi(code) => write!(f, "Windows API error: {}", code),
            AppError::Io(e) => write!(f, "IO error: {}", e),
            AppError::Database(e) => write!(f, "Database error: {}", e),
            AppError::Regex(e) => write!(f, "Regex error: {}", e),
            AppError::InvalidInput(s) => write!(f, "Invalid input: {}", s),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e)
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Database(e)
    }
}

impl From<regex::Error> for AppError {
    fn from(e: regex::Error) -> Self {
        AppError::Regex(e)
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
