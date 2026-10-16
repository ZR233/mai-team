//! 期望环境与实际环境的差异模型与展示。
//!
//! 差异条目本身只是数据；产生差异的比对逻辑在 environment::compare。

use std::fmt;

use uuid::Uuid;

use crate::mount::ContainerMountType;

/// 期望环境与实际环境的差异集合；为空表示可以复用现有容器。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvironmentDiff {
    entries: Vec<EnvironmentDiffEntry>,
}

impl EnvironmentDiff {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[EnvironmentDiffEntry] {
        &self.entries
    }

    pub(crate) fn push(&mut self, entry: EnvironmentDiffEntry) {
        self.entries.push(entry);
    }
}

impl fmt::Display for EnvironmentDiff {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.entries.is_empty() {
            return formatter.write_str("no differences");
        }
        let rendered = self
            .entries
            .iter()
            .map(|entry| entry.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        formatter.write_str(&rendered)
    }
}

/// 一条差异；每条都对应一个可以独立解释的核对点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentDiffEntry {
    PlanInvalid {
        reason: String,
    },
    AgentMismatch {
        expected: String,
        actual: Option<String>,
    },
    /// 实际命中的是 sidecar，不能当作 Agent 执行容器复用。
    UnexpectedSidecar {
        kind: Option<String>,
    },
    GenerationMismatch {
        expected: Option<Uuid>,
        actual: Option<Uuid>,
    },
    ImageMismatch {
        expected: String,
        actual: String,
    },
    /// 创建时使用的镜像引用不同（例如恢复时误用了基础镜像而不是快照镜像）。
    ImageReferenceMismatch {
        expected: String,
        actual: String,
    },
    MissingMount {
        target: String,
    },
    UnexpectedMount {
        target: String,
        mount_type: ContainerMountType,
    },
    MountTypeMismatch {
        target: String,
        expected: ContainerMountType,
        actual: ContainerMountType,
    },
    MountSourceMismatch {
        target: String,
        expected: String,
        actual: String,
    },
    MountAccessMismatch {
        target: String,
        expected_read_only: bool,
        actual_read_only: bool,
    },
    MountPropagationMismatch {
        target: String,
        expected: String,
        actual: String,
    },
    /// 引擎报告该 bind 不是严格只读（`bind-recursive=readonly` 未生效）。
    MountRecursiveReadOnlyMismatch {
        target: String,
        actual: Option<bool>,
    },
    /// 引擎没有报告递归只读状态，无法核验，按未通过处理。
    MountRecursiveReadOnlyUnverified {
        target: String,
    },
    MountSubpathMismatch {
        target: String,
        expected: Option<String>,
        actual: Option<String>,
    },
    NetworkMismatch {
        expected: String,
        actual: Option<String>,
    },
    PrivilegedEnabled {
        actual: bool,
    },
    CapabilityAdded {
        caps: Vec<String>,
    },
    CapabilityDropMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    SecurityOptionMissing {
        option: String,
    },
    DockerSocketExposed {
        target: String,
    },
    MemoryLimitMismatch {
        expected: Option<u64>,
        actual: Option<i64>,
    },
    CpuLimitMismatch {
        expected: Option<i64>,
        actual: Option<i64>,
    },
    PidsLimitMismatch {
        expected: Option<i64>,
        actual: Option<i64>,
    },
}

impl fmt::Display for EnvironmentDiffEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanInvalid { reason } => write!(formatter, "环境计划本身无效：{reason}"),
            Self::AgentMismatch { expected, actual } => write!(
                formatter,
                "容器归属 Agent 不匹配：期望 {expected}，实际 {}",
                describe_optional(actual.as_deref())
            ),
            Self::UnexpectedSidecar { kind } => write!(
                formatter,
                "命中的是 sidecar 容器（kind={}），不是 Agent 执行容器",
                describe_optional(kind.as_deref())
            ),
            Self::GenerationMismatch { expected, actual } => write!(
                formatter,
                "environment generation 不匹配：期望 {}，实际 {}",
                describe_optional(expected.map(|value| value.to_string()).as_deref()),
                describe_optional(actual.map(|value| value.to_string()).as_deref())
            ),
            Self::ImageMismatch { expected, actual } => {
                write!(formatter, "生效镜像不匹配：期望 {expected}，实际 {actual}")
            }
            Self::ImageReferenceMismatch { expected, actual } => write!(
                formatter,
                "创建镜像引用不匹配：期望 {expected}，实际 {actual}"
            ),
            Self::MissingMount { target } => write!(formatter, "缺少期望挂载 {target}"),
            Self::UnexpectedMount { target, mount_type } => {
                write!(formatter, "存在计划外的 {mount_type} 挂载 {target}")
            }
            Self::MountTypeMismatch {
                target,
                expected,
                actual,
            } => write!(
                formatter,
                "挂载类型不匹配 {target}：期望 {expected}，实际 {actual}"
            ),
            Self::MountSourceMismatch {
                target,
                expected,
                actual,
            } => write!(
                formatter,
                "挂载来源不匹配 {target}：期望 {expected}，实际 {actual}"
            ),
            Self::MountAccessMismatch {
                target,
                expected_read_only,
                actual_read_only,
            } => write!(
                formatter,
                "挂载访问模式不匹配 {target}：期望{}，实际{}",
                describe_access(*expected_read_only),
                describe_access(*actual_read_only)
            ),
            Self::MountPropagationMismatch {
                target,
                expected,
                actual,
            } => write!(
                formatter,
                "bind 传播模式不匹配 {target}：期望 {expected}，实际 {actual}"
            ),
            Self::MountRecursiveReadOnlyMismatch { target, actual } => write!(
                formatter,
                "bind 递归只读未生效 {target}：引擎报告 {}",
                describe_optional(actual.map(|value| if value { "readonly" } else { "disabled" }))
            ),
            Self::MountRecursiveReadOnlyUnverified { target } => write!(
                formatter,
                "bind 递归只读状态未核验 {target}：引擎未报告 bind-recursive 结果"
            ),
            Self::MountSubpathMismatch {
                target,
                expected,
                actual,
            } => write!(
                formatter,
                "volume subpath 不匹配 {target}：期望 {}，实际 {}",
                describe_optional(expected.as_deref()),
                describe_optional(actual.as_deref())
            ),
            Self::NetworkMismatch { expected, actual } => write!(
                formatter,
                "网络模式不匹配：期望 {expected}，实际 {}",
                describe_optional(actual.as_deref())
            ),
            Self::PrivilegedEnabled { actual } => {
                write!(
                    formatter,
                    "容器处于 privileged 状态（实际 {actual}），期望非特权"
                )
            }
            Self::CapabilityAdded { caps } => {
                write!(formatter, "容器额外增加了能力：{}", caps.join(","))
            }
            Self::CapabilityDropMismatch { expected, actual } => write!(
                formatter,
                "能力丢弃列表不匹配：期望 [{}]，实际 [{}]",
                expected.join(","),
                actual.join(",")
            ),
            Self::SecurityOptionMissing { option } => {
                write!(formatter, "缺少安全选项 {option}")
            }
            Self::DockerSocketExposed { target } => {
                write!(formatter, "容器暴露了 Docker socket：{target}")
            }
            Self::MemoryLimitMismatch { expected, actual } => write!(
                formatter,
                "内存限制不匹配：期望 {}，实际 {}",
                describe_optional(expected.map(|value| value.to_string()).as_deref()),
                describe_optional(actual.map(|value| value.to_string()).as_deref())
            ),
            Self::CpuLimitMismatch { expected, actual } => write!(
                formatter,
                "CPU 限制不匹配：期望 {}，实际 {}",
                describe_optional(expected.map(|value| value.to_string()).as_deref()),
                describe_optional(actual.map(|value| value.to_string()).as_deref())
            ),
            Self::PidsLimitMismatch { expected, actual } => write!(
                formatter,
                "pids 限制不匹配：期望 {}，实际 {}",
                describe_optional(expected.map(|value| value.to_string()).as_deref()),
                describe_optional(actual.map(|value| value.to_string()).as_deref())
            ),
        }
    }
}

fn describe_optional(value: Option<&str>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "无".to_string(),
    }
}

fn describe_access(read_only: bool) -> &'static str {
    if read_only { "只读" } else { "可写" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    #[test]
    fn renders_every_diff_entry() {
        let generation = Uuid::parse_str("8f058410-c69e-45d6-a10d-d204db72b30a").expect("uuid");
        let entries = vec![
            EnvironmentDiffEntry::PlanInvalid {
                reason: "no effective image".to_string(),
            },
            EnvironmentDiffEntry::AgentMismatch {
                expected: "agent-1".to_string(),
                actual: None,
            },
            EnvironmentDiffEntry::UnexpectedSidecar { kind: None },
            EnvironmentDiffEntry::GenerationMismatch {
                expected: Some(generation),
                actual: None,
            },
            EnvironmentDiffEntry::ImageMismatch {
                expected: "sha256:expected".to_string(),
                actual: "sha256:actual".to_string(),
            },
            EnvironmentDiffEntry::ImageReferenceMismatch {
                expected: "ghcr.io/example/agent:latest".to_string(),
                actual: "ghcr.io/example/other:latest".to_string(),
            },
            EnvironmentDiffEntry::MissingMount {
                target: "/references/linux".to_string(),
            },
            EnvironmentDiffEntry::UnexpectedMount {
                target: "/references/linux".to_string(),
                mount_type: ContainerMountType::Bind,
            },
            EnvironmentDiffEntry::MountTypeMismatch {
                target: "/references/linux".to_string(),
                expected: ContainerMountType::Bind,
                actual: ContainerMountType::Volume,
            },
            EnvironmentDiffEntry::MountSourceMismatch {
                target: "/references/linux".to_string(),
                expected: "/srv/a".to_string(),
                actual: "/srv/b".to_string(),
            },
            EnvironmentDiffEntry::MountAccessMismatch {
                target: "/workspace".to_string(),
                expected_read_only: false,
                actual_read_only: true,
            },
            EnvironmentDiffEntry::MountPropagationMismatch {
                target: "/references/linux".to_string(),
                expected: "rprivate".to_string(),
                actual: "shared".to_string(),
            },
            EnvironmentDiffEntry::MountRecursiveReadOnlyMismatch {
                target: "/references/linux".to_string(),
                actual: Some(false),
            },
            EnvironmentDiffEntry::MountRecursiveReadOnlyUnverified {
                target: "/references/linux".to_string(),
            },
            EnvironmentDiffEntry::MountSubpathMismatch {
                target: "/project/repo".to_string(),
                expected: Some("review-contexts/run-1/repo".to_string()),
                actual: None,
            },
            EnvironmentDiffEntry::NetworkMismatch {
                expected: "host".to_string(),
                actual: Some("bridge".to_string()),
            },
            EnvironmentDiffEntry::PrivilegedEnabled { actual: true },
            EnvironmentDiffEntry::CapabilityAdded {
                caps: vec!["SYS_ADMIN".to_string()],
            },
            EnvironmentDiffEntry::CapabilityDropMismatch {
                expected: Vec::new(),
                actual: vec!["ALL".to_string()],
            },
            EnvironmentDiffEntry::SecurityOptionMissing {
                option: "no-new-privileges".to_string(),
            },
            EnvironmentDiffEntry::DockerSocketExposed {
                target: "/var/run/docker.sock".to_string(),
            },
            EnvironmentDiffEntry::MemoryLimitMismatch {
                expected: Some(8589934592),
                actual: None,
            },
            EnvironmentDiffEntry::CpuLimitMismatch {
                expected: Some(4000000000),
                actual: None,
            },
            EnvironmentDiffEntry::PidsLimitMismatch {
                expected: Some(2048),
                actual: None,
            },
        ];
        let mut diff = EnvironmentDiff::default();
        let expected_entries = entries.len();
        for entry in entries {
            diff.push(entry);
        }

        assert!(!diff.is_empty());
        assert_eq!(diff.entries().len(), expected_entries);
        assert!(diff.to_string().contains("/references/linux"));
        assert!(
            diff.to_string()
                .contains("容器处于 privileged 状态（实际 true）")
        );
        assert_eq!(EnvironmentDiff::default().to_string(), "no differences");
    }
}
