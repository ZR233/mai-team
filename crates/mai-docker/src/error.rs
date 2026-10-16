use thiserror::Error;

use crate::environment::EnvironmentDiff;

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker is not available: {0}")]
    NotAvailable(String),
    #[error("docker command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("utf8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid docker image: {0}")]
    InvalidImage(String),
    #[error("invalid docker mount: {0}")]
    InvalidMount(String),
    #[error("invalid workspace export: {0}")]
    InvalidWorkspaceExport(String),
    #[error("docker command cancelled")]
    Cancelled,
    /// 主机平台、Docker daemon 或内核不满足本模块要求的严格只读挂载能力。
    #[error("unsupported environment capability: {0}")]
    UnsupportedCapability(String),
    /// 已存在的容器与期望环境不一致，调用方必须先执行换代，本层不隐式删除。
    #[error("existing environment requires reconfiguration: {0}")]
    EnvironmentReconfigurationRequired(EnvironmentDiff),
    /// 实际存在的挂载既不属于期望计划，也不属于可保留的卷或参考命名空间。
    #[error("unexpected existing mount `{0}` cannot be preserved or dropped safely")]
    UnexpectedExistingMount(String),
    /// 换代候选容器名已被占用，必须先定向收敛旧 generation。
    #[error("environment container name `{0}` is already in use")]
    EnvironmentNameConflict(String),
    /// 定向操作要求容器处于特定状态，实际状态不满足。
    #[error("container `{0}` is not in the required state")]
    EnvironmentStateMismatch(String),
    /// 定向清理/核验找不到指定容器；未知容器一律不删除。
    #[error("unknown container `{0}`")]
    UnknownContainer(String),
    /// 指定容器属于受保护资源（旧候选/旧快照的复用来源），不得被本次清理删除。
    #[error("container `{0}` is protected by the current operation")]
    ProtectedContainer(String),
}

pub type Result<T> = std::result::Result<T, DockerError>;
