//! 部署配置独立于产品库；管理 API 不能扩大允许根。

use crate::{ReferenceDeploymentPolicy, ReferenceError, Result};
use mai_protocol::ReferenceErrorCode;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentFile {
    allowed_roots: Vec<PathBuf>,
    #[serde(default = "trusted_root")]
    trusted_owner_uids: Vec<u32>,
    #[serde(default = "max_bindings")]
    max_bindings_per_project: u16,
}
fn trusted_root() -> Vec<u32> {
    vec![0]
}
fn max_bindings() -> u16 {
    16
}

pub fn load_reference_deployment_policy(path: &Path) -> Result<ReferenceDeploymentPolicy> {
    let data = std::fs::read_to_string(path).map_err(|error| {
        ReferenceError::from_io(
            ReferenceErrorCode::ReferenceStorageUnavailable,
            "无法读取参考源部署策略",
            error,
        )
    })?;
    let config: DeploymentFile = toml::from_str(&data).map_err(|error| {
        ReferenceError::new(
            ReferenceErrorCode::InvalidRequest,
            "参考源部署策略不是严格 TOML 配置",
        )
        .with_cause(error)
    })?;
    if config.trusted_owner_uids.is_empty()
        || config.max_bindings_per_project == 0
        || config.max_bindings_per_project > 16
    {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidRequest,
            "部署策略必须指定可信 UID，且单项目限制在 1 到 16 之间",
        ));
    }
    Ok(ReferenceDeploymentPolicy::new(config.allowed_roots)?
        .with_trusted_owner_uids(config.trusted_owner_uids)
        .with_max_bindings_per_project(config.max_bindings_per_project))
}
