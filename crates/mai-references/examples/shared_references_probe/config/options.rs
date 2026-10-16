//! 驱动配置里的枚举取值（子命令、场景、信任模式、覆盖范围、摘要范围）。

use super::*;

/// 驱动子命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Seed,
    Apply,
    Status,
    Recover,
    Clear,
    Delete,
}

impl Command {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "seed" => Ok(Self::Seed),
            "apply" => Ok(Self::Apply),
            "status" => Ok(Self::Status),
            "recover" => Ok(Self::Recover),
            "clear" => Ok(Self::Clear),
            "delete" => Ok(Self::Delete),
            other => err(
                "unknown_command",
                format!("未知命令 `{other}`；只支持 seed/apply/status/recover/clear/delete"),
            ),
        }
    }

    /// 写入证据的命令名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Apply => "apply",
            Self::Status => "status",
            Self::Recover => "recover",
            Self::Clear => "clear",
            Self::Delete => "delete",
        }
    }
}

/// `apply` 本轮要建立的目标配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    /// 绑定“Linux 快照 + A 项目私有夹具”，B 项目与非项目 Agent 保持空绑定。
    Bind,
    /// 只改说明（挂载指纹不变）：容器 ID/generation 必须不变，模型可见清单必须更新。
    MetadataOnly,
    /// 绑定清空：旧的真实挂载必须消失，且不能复用旧容器。
    Clear,
    /// 不改配置，重复 ensure 两次并比较容器身份（配合跨进程重启）。
    Reuse,
    /// 应用途中再保存更新的目标，验证旧目标不能覆盖最新目标且最终收敛。
    Concurrent,
}

impl Scenario {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "bind" => Ok(Self::Bind),
            "metadata_only" => Ok(Self::MetadataOnly),
            "clear" => Ok(Self::Clear),
            "reuse" => Ok(Self::Reuse),
            "concurrent" => Ok(Self::Concurrent),
            other => err(
                "unknown_scenario",
                format!("未知场景 `{other}`；只支持 bind/metadata_only/clear/reuse/concurrent"),
            ),
        }
    }

    /// 写入证据的场景名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::MetadataOnly => "metadata_only",
            Self::Clear => "clear",
            Self::Reuse => "reuse",
            Self::Concurrent => "concurrent",
        }
    }
}

/// 源归属信任要求的选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustChoice {
    /// 生产口径：源根与全部祖先由可信 UID 持有，且不允许组/其他用户写入。
    TrustedOwnerAndPermissions,
    /// 仅用于离线夹具：跳过归属与权限检查，路径/类型/链接检查仍然执行。
    Unrestricted,
}

impl TrustChoice {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "trusted_owner_and_permissions" | "trusted" => Ok(Self::TrustedOwnerAndPermissions),
            "unrestricted" => Ok(Self::Unrestricted),
            other => err(
                "unknown_trust_mode",
                format!(
                    "未知信任模式 `{other}`；只支持 trusted_owner_and_permissions/unrestricted"
                ),
            ),
        }
    }
}

/// 物理环境覆盖范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionChoice {
    /// 只给 4 个必须的 Agent 建真实环境：A 维护者、A child、B executor、非项目 executor。
    Canonical,
    /// 给全部 8 个夹具 Agent 建真实环境。
    All,
}

impl ProvisionChoice {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "canonical" => Ok(Self::Canonical),
            "all" => Ok(Self::All),
            other => err(
                "unknown_provision_mode",
                format!("未知环境覆盖模式 `{other}`；只支持 canonical/all"),
            ),
        }
    }
}

/// 宿主内容摘要的覆盖范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestScope {
    /// 只在写入尝试前后摘要私有夹具（整棵 Linux 树由真实验证器的摘要跨轮比较覆盖）。
    Fixture,
    /// 同时摘要 Linux 快照（慢，但证据最完整）。
    All,
    /// 不做本轮前后摘要，只复用真实验证器记录的内容摘要。
    None,
}

impl DigestScope {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "fixture" => Ok(Self::Fixture),
            "all" => Ok(Self::All),
            "none" => Ok(Self::None),
            other => err(
                "unknown_digest_scope",
                format!("未知摘要范围 `{other}`；只支持 fixture/all/none"),
            ),
        }
    }
}
