//! 驱动错误：稳定短码 + 中文说明，便于 root 在证据里定位失败步骤。

/// 驱动结果类型。
pub type Result<T> = std::result::Result<T, DriverError>;

/// 带稳定短码的驱动错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverError {
    code: String,
    message: String,
}

impl DriverError {
    /// 构造错误。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// 稳定短码。
    pub fn code(&self) -> &str {
        &self.code
    }

    /// 中文说明。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DriverError {}

/// 构造错误。
pub fn error(code: impl Into<String>, message: impl Into<String>) -> DriverError {
    DriverError::new(code, message)
}

impl From<mai_runtime::RuntimeError> for DriverError {
    fn from(cause: mai_runtime::RuntimeError) -> Self {
        error("runtime_error", cause.to_string())
    }
}

impl From<mai_store::StoreError> for DriverError {
    fn from(cause: mai_store::StoreError) -> Self {
        error("store_error", cause.to_string())
    }
}

impl From<mai_references::ReferenceError> for DriverError {
    fn from(cause: mai_references::ReferenceError) -> Self {
        error(cause.code().to_string(), cause.to_string())
    }
}

impl From<mai_docker::DockerError> for DriverError {
    fn from(cause: mai_docker::DockerError) -> Self {
        error("docker_error", cause.to_string())
    }
}
