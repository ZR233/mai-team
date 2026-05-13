use thiserror::Error;

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid skill config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, SkillError>;
