//! 驱动的统一错误：稳定机器码 + 中文说明；机器码进 JSON 证据，说明给人看。
//!
//! 领域、存储与 Docker 的错误都在这里转换成同一个形状，命令层因此可以用 `?` 直接
//! 传播，同时保证失败一定会以非零退出码结束，不会被当作成功证据。

use mai_docker::DockerError;
use mai_references::ReferenceError;

/// 驱动错误：`code` 是稳定机器码，`message` 是中文说明。
#[derive(Debug)]
pub struct ProbeError {
    code: String,
    message: String,
}

/// 驱动内部结果类型。
pub type Result<T> = std::result::Result<T, ProbeError>;

impl ProbeError {
    /// 用机器码与中文说明构造错误。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// 稳定机器码。
    pub fn code(&self) -> &str {
        &self.code
    }

    /// 中文说明。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProbeError {}

impl From<ReferenceError> for ProbeError {
    fn from(error: ReferenceError) -> Self {
        Self::new(error.code().to_string(), error.message().to_string())
    }
}

impl From<mai_store::StoreError> for ProbeError {
    fn from(error: mai_store::StoreError) -> Self {
        Self::new("reference_storage_unavailable", error.to_string())
    }
}

impl From<DockerError> for ProbeError {
    fn from(error: DockerError) -> Self {
        Self::new("docker_command_failed", error.to_string())
    }
}

impl From<std::io::Error> for ProbeError {
    fn from(error: std::io::Error) -> Self {
        Self::new("io_error", error.to_string())
    }
}

impl From<serde_json::Error> for ProbeError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("json_error", error.to_string())
    }
}

/// 命令失败时带出是哪个命令失败，便于 root 在 JSON 里直接定位。
#[derive(Debug)]
pub struct CommandFailure {
    pub command: String,
    pub error: ProbeError,
}

/// 构造错误的便捷函数，让领域调用点读起来像断言。
pub fn err<T>(code: impl Into<String>, message: impl Into<String>) -> Result<T> {
    Err(ProbeError::new(code, message))
}
