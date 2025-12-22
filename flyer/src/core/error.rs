use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("SSH error: {0}")]
    Ssh(#[from] ssh2::Error),
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("Navigation: {0}")]
    Navigation(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
