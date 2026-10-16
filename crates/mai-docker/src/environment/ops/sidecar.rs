//! MCP sidecar 复用前的契约核对。
//!
//! sidecar 的卷来自 `--volumes-from`，参考路径来自显式只读挂载，两者都必须真的对应
//! 当前的源容器与计划；generation 相同不足以证明这一点，所以这里逐项核对身份、
//! 源容器、镜像 ID、挂载集合、docker socket、特权、capability 与资源契约。

use crate::args::HOST_NETWORK;
use crate::environment::EnvironmentDiff;
use crate::environment::compare::compare_required_mounts;
use crate::environment::plan::AgentMcpSidecarPlan;
use crate::inspect::{ContainerMountFacts, ContainerVerificationFacts};
use crate::mount::{ContainerMountType, REFERENCE_MOUNT_ROOT};
use crate::naming::AGENT_MCP_SIDECAR_KIND;

/// MCP sidecar 不能复用的原因。
///
/// 类别是值：调用方（以及日志）不需要解析诊断文本就能知道是哪一条契约被违反。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpSidecarIssue {
    /// 标签身份不匹配（不是本 Agent／本 generation 的 agent-mcp sidecar）。
    IdentityMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    /// 计划的只读挂载缺失或形状不符（source／只读／递归／rprivate／subpath）。
    PlannedMountMismatch { diff: EnvironmentDiff },
    /// 出现计划外的 `/references/*` 挂载：sidecar 不得比计划多挂任何参考路径。
    UnexpectedReferenceMount { target: String },
    /// 挂载了 docker socket。
    DockerSocketExposed { target: String },
    /// 源容器身份不符：记录/时间关系说明它不是当前源容器的 sidecar。
    SourceContainerMismatch { reason: String },
    /// 源容器的卷没有按 `--volumes-from` 的语义出现在 sidecar 上。
    SourceVolumeMismatch { target: String, reason: String },
    /// 镜像 ID 与当前 sidecar 镜像不一致（镜像被重打标签时不复用旧环境）。
    ImageMismatch { expected: String, actual: String },
    /// 与 sidecar 创建契约（args.rs）不符的安全／资源字段。
    SidecarContractViolation {
        field: String,
        expected: String,
        actual: String,
    },
}

impl McpSidecarIssue {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::IdentityMismatch {
                field,
                expected,
                actual,
            } => format!("sidecar 身份标签 {field} 不匹配：期望 {expected}，实际 {actual}"),
            Self::PlannedMountMismatch { diff } => {
                format!("sidecar 的计划内挂载不符：{diff}")
            }
            Self::UnexpectedReferenceMount { target } => {
                format!("sidecar 存在计划外的参考挂载 {target}")
            }
            Self::DockerSocketExposed { target } => {
                format!("sidecar 暴露了 Docker socket：{target}")
            }
            Self::SourceContainerMismatch { reason } => {
                format!("sidecar 与源容器不匹配：{reason}")
            }
            Self::SourceVolumeMismatch { target, reason } => {
                format!("sidecar 的 --volumes-from 结果不符 {target}：{reason}")
            }
            Self::ImageMismatch { expected, actual } => {
                format!("sidecar 生效镜像不匹配：期望 {expected}，实际 {actual}")
            }
            Self::SidecarContractViolation {
                field,
                expected,
                actual,
            } => format!("sidecar 契约字段 {field} 不符：期望 {expected}，实际 {actual}"),
        }
    }
}

/// MCP sidecar 的创建契约（与 args.rs 的 `create_agent_mcp_sidecar_container_args` 一一对应）。
///
/// 两边必须一起改：修改 sidecar 创建参数时，这里的核对与
/// `sidecar_contract_matches_creation_args` 测试会失败，提醒同步核验逻辑，
/// 避免"核验的 sidecar 形状"与"真实创建的 sidecar 形状"悄悄漂移。
#[derive(Debug, Clone, PartialEq, Eq)]
struct McpSidecarContract {
    network_mode: &'static str,
    memory_bytes: Option<i64>,
    nano_cpus: Option<i64>,
    pids_limit: Option<i64>,
    cap_drop: Vec<String>,
}

fn mcp_sidecar_contract() -> McpSidecarContract {
    McpSidecarContract {
        network_mode: HOST_NETWORK,
        memory_bytes: None,
        nano_cpus: None,
        pids_limit: None,
        cap_drop: Vec::new(),
    }
}

/// sidecar 复用核对：返回所有不符合项（为空表示可以复用）。
///
/// 只复用"就是按当前契约、从当前源容器创建出来的" sidecar：generation 相同不足以证明
/// 这一点，所以还要核对源容器身份、`--volumes-from` 的真实结果、镜像 ID、参考挂载集合
/// （既不能少，也不能多）、docker socket、特权、capability 与资源契约。
pub(crate) fn sidecar_reuse_issues(
    plan: &AgentMcpSidecarPlan,
    sidecar: &ContainerVerificationFacts,
    source: &ContainerVerificationFacts,
    expected_image_id: &str,
) -> Vec<McpSidecarIssue> {
    let mut issues = Vec::new();
    let environment = &sidecar.environment;

    if environment.agent_id.as_deref() != Some(plan.agent_id()) {
        issues.push(McpSidecarIssue::IdentityMismatch {
            field: "mai.team.agent".to_string(),
            expected: plan.agent_id().to_string(),
            actual: describe_optional(environment.agent_id.as_deref()),
        });
    }
    if environment.generation != Some(plan.generation()) {
        issues.push(McpSidecarIssue::IdentityMismatch {
            field: "mai.team.generation".to_string(),
            expected: plan.generation().to_string(),
            actual: describe_optional(
                environment
                    .generation
                    .map(|generation| generation.to_string())
                    .as_deref(),
            ),
        });
    }
    let sidecar_kind = environment.sidecar_kind.as_deref();
    if !environment.sidecar || sidecar_kind != Some(AGENT_MCP_SIDECAR_KIND) {
        issues.push(McpSidecarIssue::IdentityMismatch {
            field: "mai.team.sidecar.kind".to_string(),
            expected: AGENT_MCP_SIDECAR_KIND.to_string(),
            actual: describe_optional(sidecar_kind),
        });
    }

    // 源容器身份：标签存在时必须一致；创建时间必须晚于源容器（否则它不可能复制到
    // 当前源容器的卷）。时间戳无法归一化时按未核验处理，不假装通过。
    if let Some(actual_source) = sidecar.sidecar_source_container_id.as_deref()
        && actual_source != plan.source_container_id()
    {
        issues.push(McpSidecarIssue::SourceContainerMismatch {
            reason: format!(
                "sidecar 记录的源容器 `{actual_source}` 与计划的 `{}` 不一致",
                plan.source_container_id()
            ),
        });
    }
    if sidecar.created_after(source) == Some(false) {
        issues.push(McpSidecarIssue::SourceContainerMismatch {
            reason: format!(
                "sidecar 早于源容器 {} 创建，不可能带出它当前的卷集合",
                source.id()
            ),
        });
    }

    if environment.image_id != expected_image_id {
        issues.push(McpSidecarIssue::ImageMismatch {
            expected: expected_image_id.to_string(),
            actual: environment.image_id.clone(),
        });
    }

    // 计划内的只读挂载必须逐项一致（source／只读／递归只读／rprivate／subpath）。
    let planned = compare_required_mounts(plan.mounts(), environment);
    if !planned.is_empty() {
        issues.push(McpSidecarIssue::PlannedMountMismatch { diff: planned });
    }

    // 计划外的参考挂载一律不允许：只读的额外参考路径同样是多余的授权面。
    for mount in &environment.mounts {
        if !within_reference_root(&mount.target) {
            continue;
        }
        if plan
            .mounts()
            .iter()
            .any(|planned| planned.target() == mount.target)
        {
            continue;
        }
        issues.push(McpSidecarIssue::UnexpectedReferenceMount {
            target: mount.target.clone(),
        });
    }

    for mount in &environment.mounts {
        if exposes_docker_socket(mount) {
            issues.push(McpSidecarIssue::DockerSocketExposed {
                target: mount.target.clone(),
            });
        }
    }

    if environment.host.privileged {
        issues.push(McpSidecarIssue::SidecarContractViolation {
            field: "privileged".to_string(),
            expected: "false".to_string(),
            actual: "true".to_string(),
        });
    }
    if !environment.host.cap_add.is_empty() {
        issues.push(McpSidecarIssue::SidecarContractViolation {
            field: "cap_add".to_string(),
            expected: "[]".to_string(),
            actual: environment.host.cap_add.join(","),
        });
    }

    let contract = mcp_sidecar_contract();
    if environment.host.network_mode.as_deref() != Some(contract.network_mode) {
        issues.push(McpSidecarIssue::SidecarContractViolation {
            field: "network_mode".to_string(),
            expected: contract.network_mode.to_string(),
            actual: describe_optional(environment.host.network_mode.as_deref()),
        });
    }
    check_sidecar_limit(
        &mut issues,
        "memory_limit",
        contract.memory_bytes,
        environment.host.memory_bytes.filter(|value| *value > 0),
    );
    check_sidecar_limit(
        &mut issues,
        "cpu_limit",
        contract.nano_cpus,
        environment.host.nano_cpus.filter(|value| *value > 0),
    );
    check_sidecar_limit(
        &mut issues,
        "pids_limit",
        contract.pids_limit,
        environment.host.pids_limit.filter(|value| *value > 0),
    );
    if environment.host.cap_drop != contract.cap_drop {
        issues.push(McpSidecarIssue::SidecarContractViolation {
            field: "cap_drop".to_string(),
            expected: format!("{:?}", contract.cap_drop),
            actual: format!("{:?}", environment.host.cap_drop),
        });
    }

    // `--volumes-from` 的真实结果：源容器的每个卷都必须在 sidecar 上以同名、同读写模式出现。
    // sidecar 自己镜像声明的额外卷允许存在，但不能与源容器的卷冲突。
    for mount in &source.environment.mounts {
        if mount.mount_type != ContainerMountType::Volume {
            continue;
        }
        let expected_name = mount
            .volume_name
            .as_deref()
            .unwrap_or(mount.source.as_str());
        match environment.mount_at(&mount.target) {
            None => issues.push(McpSidecarIssue::SourceVolumeMismatch {
                target: mount.target.clone(),
                reason: format!("sidecar 缺少源容器的卷 `{expected_name}`"),
            }),
            Some(actual) => {
                if actual.mount_type != ContainerMountType::Volume {
                    issues.push(McpSidecarIssue::SourceVolumeMismatch {
                        target: mount.target.clone(),
                        reason: format!(
                            "sidecar 在该目标上是 {} 而不是源容器的卷",
                            actual.mount_type
                        ),
                    });
                    continue;
                }
                let actual_name = actual
                    .volume_name
                    .as_deref()
                    .unwrap_or(actual.source.as_str());
                if actual_name != expected_name {
                    issues.push(McpSidecarIssue::SourceVolumeMismatch {
                        target: mount.target.clone(),
                        reason: format!("卷名不同：期望 `{expected_name}`，实际 `{actual_name}`"),
                    });
                }
                if actual.read_only != mount.read_only {
                    issues.push(McpSidecarIssue::SourceVolumeMismatch {
                        target: mount.target.clone(),
                        reason: format!(
                            "读写模式不同：源容器 {}，sidecar {}",
                            describe_access(mount.read_only),
                            describe_access(actual.read_only)
                        ),
                    });
                }
            }
        }
    }

    issues
}

fn check_sidecar_limit(
    issues: &mut Vec<McpSidecarIssue>,
    field: &str,
    expected: Option<i64>,
    actual: Option<i64>,
) {
    if expected != actual {
        issues.push(McpSidecarIssue::SidecarContractViolation {
            field: field.to_string(),
            expected: describe_optional(expected.map(|value| value.to_string()).as_deref()),
            actual: describe_optional(actual.map(|value| value.to_string()).as_deref()),
        });
    }
}

pub(crate) fn sidecar_issue_summary(issues: &[McpSidecarIssue]) -> String {
    issues
        .iter()
        .map(|issue| issue.message())
        .collect::<Vec<_>>()
        .join("; ")
}

fn within_reference_root(target: &str) -> bool {
    target == REFERENCE_MOUNT_ROOT || target.starts_with(&format!("{REFERENCE_MOUNT_ROOT}/"))
}

fn exposes_docker_socket(mount: &ContainerMountFacts) -> bool {
    mount.source.ends_with("/docker.sock") || mount.target == "/var/run/docker.sock"
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
    use crate::args::create_agent_mcp_sidecar_container_args;
    use crate::inspect::{ContainerEnvironmentFacts, ContainerHostFacts};
    use crate::mount::{ContainerMount, ReadOnlyBindMount};
    use uuid::Uuid;

    const SIDECAR_IMAGE: &str = "ghcr.io/zr233/mai-team-sidecar:latest";
    const SIDECAR_IMAGE_ID: &str = "sha256:sidecar";
    const AGENT_CONTAINER: &str = "agent-container-1";

    #[test]
    fn matching_sidecar_is_reusable() {
        let plan = sidecar_plan(vec![reference_mount("linux")]);
        let sidecar = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume(), reference_fact("linux")],
        );
        let source = source_facts("2026-10-15T07:23:40Z", vec![workspace_volume()]);

        assert_eq!(
            sidecar_reuse_issues(&plan, &sidecar, &source, SIDECAR_IMAGE_ID),
            Vec::new()
        );
    }

    #[test]
    fn sidecar_with_an_extra_reference_mount_blocks_reuse() {
        let plan = sidecar_plan(vec![reference_mount("linux")]);
        let sidecar = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![
                workspace_volume(),
                reference_fact("linux"),
                // 计划外但仍然是只读的参考挂载：同样不能复用，否则 sidecar 比计划多一个授权面。
                reference_fact("other"),
            ],
        );
        let source = source_facts("2026-10-15T07:23:40Z", vec![workspace_volume()]);

        assert_eq!(
            sidecar_reuse_issues(&plan, &sidecar, &source, SIDECAR_IMAGE_ID),
            vec![McpSidecarIssue::UnexpectedReferenceMount {
                target: "/references/other".to_string(),
            }]
        );
    }

    #[test]
    fn sidecar_missing_or_writable_or_unverified_reference_mount_blocks_reuse() {
        let plan = sidecar_plan(vec![reference_mount("linux")]);
        let source = source_facts("2026-10-15T07:23:40Z", vec![workspace_volume()]);

        let missing = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume()],
        );
        let issues = sidecar_reuse_issues(&plan, &missing, &source, SIDECAR_IMAGE_ID);
        assert!(matches!(
            issues.as_slice(),
            [McpSidecarIssue::PlannedMountMismatch { diff }] if !diff.is_empty()
        ));

        let mut writable = reference_fact("linux");
        writable.read_only = false;
        let sidecar = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume(), writable],
        );
        assert!(matches!(
            sidecar_reuse_issues(&plan, &sidecar, &source, SIDECAR_IMAGE_ID).as_slice(),
            [McpSidecarIssue::PlannedMountMismatch { diff }] if !diff.is_empty()
        ));

        let mut unverified_recursive = reference_fact("linux");
        unverified_recursive.recursive_read_only = None;
        let sidecar = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume(), unverified_recursive],
        );
        assert!(matches!(
            sidecar_reuse_issues(&plan, &sidecar, &source, SIDECAR_IMAGE_ID).as_slice(),
            [McpSidecarIssue::PlannedMountMismatch { diff }] if !diff.is_empty()
        ));
    }

    #[test]
    fn sidecar_pointing_at_another_or_older_source_container_blocks_reuse() {
        let plan = sidecar_plan(vec![reference_mount("linux")]);
        let source = source_facts("2026-10-15T07:23:40Z", vec![workspace_volume()]);

        let mut stale_source = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume(), reference_fact("linux")],
        );
        stale_source.sidecar_source_container_id = Some("agent-container-old".to_string());
        assert!(matches!(
            sidecar_reuse_issues(&plan, &stale_source, &source, SIDECAR_IMAGE_ID).as_slice(),
            [McpSidecarIssue::SourceContainerMismatch { reason }]
                if reason.contains("agent-container-old")
        ));

        // generation 相同但 sidecar 比源容器更早创建：它不可能带出当前源容器的卷集合。
        let older = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:30Z",
            vec![workspace_volume(), reference_fact("linux")],
        );
        assert!(matches!(
            sidecar_reuse_issues(&plan, &older, &source, SIDECAR_IMAGE_ID).as_slice(),
            [McpSidecarIssue::SourceContainerMismatch { reason }] if reason.contains("早于源容器")
        ));
    }

    #[test]
    fn sidecar_with_docker_socket_privilege_or_extra_capabilities_blocks_reuse() {
        let plan = sidecar_plan(Vec::new());
        let source = source_facts("2026-10-15T07:23:40Z", vec![workspace_volume()]);
        let sidecar = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![
                workspace_volume(),
                ContainerMountFacts {
                    target: "/var/run/docker.sock".to_string(),
                    mount_type: ContainerMountType::Bind,
                    source: "/var/run/docker.sock".to_string(),
                    read_only: true,
                    propagation: None,
                    recursive_read_only: None,
                    volume_name: None,
                    volume_subpath: None,
                },
            ],
        );
        let mut privileged = sidecar.clone();
        privileged.environment.host.privileged = true;
        privileged.environment.host.cap_add = vec!["SYS_ADMIN".to_string()];

        let issues = sidecar_reuse_issues(&plan, &sidecar, &source, SIDECAR_IMAGE_ID);
        assert!(issues.contains(&McpSidecarIssue::DockerSocketExposed {
            target: "/var/run/docker.sock".to_string(),
        }));

        let issues = sidecar_reuse_issues(&plan, &privileged, &source, SIDECAR_IMAGE_ID);
        assert!(issues.iter().any(|issue| matches!(
            issue,
            McpSidecarIssue::SidecarContractViolation { field, .. } if field == "privileged"
        )));
        assert!(issues.iter().any(|issue| matches!(
            issue,
            McpSidecarIssue::SidecarContractViolation { field, .. } if field == "cap_add"
        )));
        assert!(issues.contains(&McpSidecarIssue::DockerSocketExposed {
            target: "/var/run/docker.sock".to_string(),
        }));
    }

    #[test]
    fn sidecar_contract_violations_block_reuse() {
        let plan = sidecar_plan(Vec::new());
        let source = source_facts("2026-10-15T07:23:40Z", vec![workspace_volume()]);
        let mut sidecar = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume()],
        );
        sidecar.environment.host.network_mode = Some("bridge".to_string());
        sidecar.environment.host.memory_bytes = Some(1073741824);
        sidecar.environment.host.nano_cpus = Some(1000000000);
        sidecar.environment.host.pids_limit = Some(64);
        sidecar.environment.host.cap_drop = vec!["NET_RAW".to_string()];

        let issues = sidecar_reuse_issues(&plan, &sidecar, &source, SIDECAR_IMAGE_ID);
        for field in [
            "network_mode",
            "memory_limit",
            "cpu_limit",
            "pids_limit",
            "cap_drop",
        ] {
            assert!(
                issues.iter().any(|issue| matches!(
                    issue,
                    McpSidecarIssue::SidecarContractViolation { field: violated, .. }
                        if violated == field
                )),
                "缺少 {field} 的契约核对：{issues:?}"
            );
        }
    }

    #[test]
    fn sidecar_missing_a_source_volume_or_carrying_another_volume_blocks_reuse() {
        let plan = sidecar_plan(Vec::new());
        let source = source_facts(
            "2026-10-15T07:23:40Z",
            vec![
                workspace_volume(),
                volume_fact("/project/repo", "review-volume", true),
            ],
        );

        let missing = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume()],
        );
        let issues = sidecar_reuse_issues(&plan, &missing, &source, SIDECAR_IMAGE_ID);
        assert_eq!(
            issues,
            vec![McpSidecarIssue::SourceVolumeMismatch {
                target: "/project/repo".to_string(),
                reason: "sidecar 缺少源容器的卷 `review-volume`".to_string(),
            }]
        );

        // 同目标但卷名不同（典型的新旧 generation 混用）必须被拒绝。
        let wrong_name = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![
                workspace_volume(),
                volume_fact("/project/repo", "other-volume", true),
            ],
        );
        assert!(matches!(
            sidecar_reuse_issues(&plan, &wrong_name, &source, SIDECAR_IMAGE_ID).as_slice(),
            [McpSidecarIssue::SourceVolumeMismatch { target, reason }]
                if target == "/project/repo" && reason.contains("卷名不同")
        ));

        // 读写模式也必须与源容器一致。
        let wrong_access = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![
                workspace_volume(),
                volume_fact("/project/repo", "review-volume", false),
            ],
        );
        assert!(matches!(
            sidecar_reuse_issues(&plan, &wrong_access, &source, SIDECAR_IMAGE_ID).as_slice(),
            [McpSidecarIssue::SourceVolumeMismatch { target, reason }]
                if target == "/project/repo" && reason.contains("读写模式不同")
        ));
    }

    #[test]
    fn sidecar_with_another_effective_image_or_identity_labels_blocks_reuse() {
        let plan = sidecar_plan(Vec::new());
        let source = source_facts("2026-10-15T07:23:40Z", vec![workspace_volume()]);
        let sidecar = sidecar_facts(
            "sidecar-1",
            "2026-10-15T07:23:45Z",
            vec![workspace_volume()],
        );

        assert!(matches!(
            sidecar_reuse_issues(&plan, &sidecar, &source, "sha256:other").as_slice(),
            [McpSidecarIssue::ImageMismatch { .. }]
        ));

        let mut other_agent = sidecar.clone();
        other_agent.environment.agent_id = Some("agent-2".to_string());
        let mut other_generation = sidecar.clone();
        other_generation.environment.generation = None;
        let mut not_a_sidecar = sidecar.clone();
        not_a_sidecar.environment.sidecar = false;
        not_a_sidecar.environment.sidecar_kind = None;

        for facts in [other_agent, other_generation, not_a_sidecar] {
            assert!(matches!(
                sidecar_reuse_issues(&plan, &facts, &source, SIDECAR_IMAGE_ID).as_slice(),
                [McpSidecarIssue::IdentityMismatch { .. }]
            ));
        }
    }

    #[test]
    fn sidecar_contract_matches_creation_args() {
        // 契约镜像 args.rs 的创建参数：这里故意直接比较两者，创建参数一变就会失败。
        let contract = mcp_sidecar_contract();
        let args = create_agent_mcp_sidecar_container_args(
            "mai-team-agent-mcp-agent-1-g8f058410c69e",
            "agent-1",
            SIDECAR_IMAGE,
            AGENT_CONTAINER,
            generation(),
            &[reference_mount("linux")],
        );

        assert!(
            args.windows(2)
                .any(|window| window == ["--network", contract.network_mode]),
            "sidecar 契约的网络模式必须与创建参数一致"
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--volumes-from", AGENT_CONTAINER])
        );
        assert!(!args.iter().any(|arg| arg == "--memory"));
        assert!(!args.iter().any(|arg| arg == "--cpus"));
        assert!(!args.iter().any(|arg| arg == "--pids-limit"));
        assert!(!args.iter().any(|arg| arg == "--cap-drop"));
        assert!(!args.iter().any(|arg| arg == "--privileged"));
        assert_eq!(contract.memory_bytes, None);
        assert_eq!(contract.nano_cpus, None);
        assert_eq!(contract.pids_limit, None);
        assert_eq!(contract.cap_drop, Vec::<String>::new());
    }

    fn generation() -> Uuid {
        Uuid::parse_str("8f058410-c69e-45d6-a10d-d204db72b30a").expect("uuid")
    }

    fn reference_mount(alias: &str) -> ContainerMount {
        ContainerMount::ReadOnlyBind(
            ReadOnlyBindMount::for_reference("/srv/mai-references/linux/repo", alias)
                .expect("reference mount"),
        )
    }

    fn sidecar_plan(mounts: Vec<ContainerMount>) -> AgentMcpSidecarPlan {
        AgentMcpSidecarPlan::new(
            "agent-1",
            generation(),
            AGENT_CONTAINER,
            SIDECAR_IMAGE,
            mounts,
        )
        .expect("sidecar plan")
    }

    fn reference_fact(alias: &str) -> ContainerMountFacts {
        ContainerMountFacts {
            target: format!("/references/{alias}"),
            mount_type: ContainerMountType::Bind,
            source: "/srv/mai-references/linux/repo".to_string(),
            read_only: true,
            propagation: Some("rprivate".to_string()),
            recursive_read_only: Some(true),
            volume_name: None,
            volume_subpath: None,
        }
    }

    fn volume_fact(target: &str, name: &str, read_only: bool) -> ContainerMountFacts {
        ContainerMountFacts {
            target: target.to_string(),
            mount_type: ContainerMountType::Volume,
            source: name.to_string(),
            read_only,
            propagation: None,
            recursive_read_only: None,
            volume_name: Some(name.to_string()),
            volume_subpath: None,
        }
    }

    fn workspace_volume() -> ContainerMountFacts {
        volume_fact("/workspace", "mai-team-workspace-agent-1", false)
    }

    fn environment(id: &str, mounts: Vec<ContainerMountFacts>) -> ContainerEnvironmentFacts {
        ContainerEnvironmentFacts {
            id: id.to_string(),
            name: format!("mai-team-{id}"),
            state: "running".to_string(),
            running: true,
            image_reference: SIDECAR_IMAGE.to_string(),
            image_id: SIDECAR_IMAGE_ID.to_string(),
            agent_id: Some("agent-1".to_string()),
            project_id: Some("project-1".to_string()),
            generation: Some(generation()),
            sidecar: true,
            sidecar_kind: Some(AGENT_MCP_SIDECAR_KIND.to_string()),
            mounts,
            host: ContainerHostFacts {
                network_mode: Some(HOST_NETWORK.to_string()),
                privileged: false,
                cap_add: Vec::new(),
                cap_drop: Vec::new(),
                memory_bytes: None,
                nano_cpus: None,
                pids_limit: None,
                security_opt: Vec::new(),
            },
        }
    }

    fn sidecar_facts(
        id: &str,
        created: &str,
        mounts: Vec<ContainerMountFacts>,
    ) -> ContainerVerificationFacts {
        ContainerVerificationFacts {
            environment: environment(id, mounts),
            created: Some(created.to_string()),
            sidecar_source_container_id: Some(AGENT_CONTAINER.to_string()),
        }
    }

    fn source_facts(created: &str, mounts: Vec<ContainerMountFacts>) -> ContainerVerificationFacts {
        let mut environment = environment(AGENT_CONTAINER, mounts);
        environment.sidecar = false;
        environment.sidecar_kind = None;
        environment.image_reference = "ghcr.io/example/agent:latest".to_string();
        environment.image_id = "sha256:effective".to_string();
        ContainerVerificationFacts {
            environment,
            created: Some(created.to_string()),
            sidecar_source_container_id: None,
        }
    }
}
