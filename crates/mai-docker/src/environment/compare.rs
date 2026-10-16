//! 期望环境与实际环境的纯函数比对。
//!
//! 输入是最终计划与 `docker inspect` 事实，输出是结构化差异；不读取任何进程内缓存，
//! 也不因为期望挂载为空而跳过核对。

use std::collections::BTreeMap;

use crate::environment::diff::{EnvironmentDiff, EnvironmentDiffEntry};
use crate::environment::plan::AgentEnvironmentPlan;
use crate::inspect::{ContainerEnvironmentFacts, ContainerMountFacts};
use crate::mount::{ContainerMount, ContainerMountAccess};

/// 完整比对：期望集合之外的挂载同样算差异（清空引用必须真实撤销旧挂载）。
pub fn compare_environment(
    plan: &AgentEnvironmentPlan,
    facts: &ContainerEnvironmentFacts,
) -> EnvironmentDiff {
    let mut diff = EnvironmentDiff::default();

    if facts.agent_id.as_deref() != Some(plan.agent_id()) {
        diff.push(EnvironmentDiffEntry::AgentMismatch {
            expected: plan.agent_id().to_string(),
            actual: facts.agent_id.clone(),
        });
    }
    if facts.sidecar {
        diff.push(EnvironmentDiffEntry::UnexpectedSidecar {
            kind: facts.sidecar_kind.clone(),
        });
    }
    if plan.generation().is_some() && plan.generation() != facts.generation {
        diff.push(EnvironmentDiffEntry::GenerationMismatch {
            expected: plan.generation(),
            actual: facts.generation,
        });
    }
    if plan.effective_image_id() != facts.image_id {
        diff.push(EnvironmentDiffEntry::ImageMismatch {
            expected: plan.effective_image_id().to_string(),
            actual: facts.image_id.clone(),
        });
    }
    // 创建镜像引用同样必须核对：恢复时误用基础镜像而不是快照镜像会丢失已安装软件。
    if !facts.image_reference.is_empty() && plan.image_reference() != facts.image_reference {
        diff.push(EnvironmentDiffEntry::ImageReferenceMismatch {
            expected: plan.image_reference().to_string(),
            actual: facts.image_reference.clone(),
        });
    }

    match plan.creation_mounts(&[]) {
        Ok(expected_mounts) => compare_mount_sets(&expected_mounts, facts, &mut diff),
        Err(error) => diff.push(EnvironmentDiffEntry::PlanInvalid {
            reason: error.to_string(),
        }),
    }

    compare_host_config(plan, facts, &mut diff);
    diff
}

/// 只核对给定挂载是否真实生效，不要求实际环境里没有别的挂载。
///
/// 用于 MCP sidecar 这类会通过 `--volumes-from` 额外继承卷的容器：仍然必须证明
/// 参考路径是严格只读，但不能因为存在继承来的卷而判定不匹配。
pub fn compare_required_mounts(
    expected: &[ContainerMount],
    facts: &ContainerEnvironmentFacts,
) -> EnvironmentDiff {
    let mut diff = EnvironmentDiff::default();
    for mount in expected {
        match facts.mount_at(mount.target()) {
            Some(actual) => compare_one_mount(mount, actual, &mut diff),
            None => diff.push(EnvironmentDiffEntry::MissingMount {
                target: mount.target().to_string(),
            }),
        }
    }
    diff
}

fn compare_mount_sets(
    expected: &[ContainerMount],
    facts: &ContainerEnvironmentFacts,
    diff: &mut EnvironmentDiff,
) {
    let mut actual_by_target: BTreeMap<&str, &ContainerMountFacts> = BTreeMap::new();
    for mount in &facts.mounts {
        actual_by_target.insert(mount.target.as_str(), mount);
    }

    for mount in expected {
        match actual_by_target.get(mount.target()) {
            Some(actual) => compare_one_mount(mount, actual, diff),
            None => diff.push(EnvironmentDiffEntry::MissingMount {
                target: mount.target().to_string(),
            }),
        }
    }

    for mount in &facts.mounts {
        if !expected
            .iter()
            .any(|expected| expected.target() == mount.target)
        {
            diff.push(EnvironmentDiffEntry::UnexpectedMount {
                target: mount.target.clone(),
                mount_type: mount.mount_type.clone(),
            });
        }
    }
}

fn compare_one_mount(
    expected: &ContainerMount,
    actual: &ContainerMountFacts,
    diff: &mut EnvironmentDiff,
) {
    let target = expected.target().to_string();
    let expected_type = expected.mount_type();
    if actual.mount_type != expected_type {
        diff.push(EnvironmentDiffEntry::MountTypeMismatch {
            target,
            expected: expected_type,
            actual: actual.mount_type.clone(),
        });
        return;
    }

    match expected {
        ContainerMount::Volume(expected_volume) => {
            let actual_name = actual
                .volume_name
                .as_deref()
                .unwrap_or(actual.source.as_str());
            if actual_name != expected_volume.volume() {
                diff.push(EnvironmentDiffEntry::MountSourceMismatch {
                    target: target.clone(),
                    expected: expected_volume.volume().to_string(),
                    actual: actual_name.to_string(),
                });
            }
            let expected_read_only = expected_volume.access() == ContainerMountAccess::ReadOnly;
            if actual.read_only != expected_read_only {
                diff.push(EnvironmentDiffEntry::MountAccessMismatch {
                    target: target.clone(),
                    expected_read_only,
                    actual_read_only: actual.read_only,
                });
            }
            if actual.volume_subpath.as_deref() != expected_volume.volume_subpath() {
                diff.push(EnvironmentDiffEntry::MountSubpathMismatch {
                    target,
                    expected: expected_volume.volume_subpath().map(ToOwned::to_owned),
                    actual: actual.volume_subpath.clone(),
                });
            }
        }
        ContainerMount::ReadOnlyBind(expected_bind) => {
            if actual.source != expected_bind.source() {
                diff.push(EnvironmentDiffEntry::MountSourceMismatch {
                    target: target.clone(),
                    expected: expected_bind.source().to_string(),
                    actual: actual.source.clone(),
                });
            }
            if !actual.read_only {
                diff.push(EnvironmentDiffEntry::MountAccessMismatch {
                    target: target.clone(),
                    expected_read_only: true,
                    actual_read_only: false,
                });
            }
            // Docker 省略传播模式表示默认的 rprivate；其它取值都必须判为不匹配。
            let propagation = actual
                .propagation
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("rprivate");
            if propagation != "rprivate" {
                diff.push(EnvironmentDiffEntry::MountPropagationMismatch {
                    target: target.clone(),
                    expected: "rprivate".to_string(),
                    actual: propagation.to_string(),
                });
            }
            match actual.recursive_read_only {
                Some(true) => {}
                Some(false) => diff.push(EnvironmentDiffEntry::MountRecursiveReadOnlyMismatch {
                    target,
                    actual: Some(false),
                }),
                None => {
                    diff.push(EnvironmentDiffEntry::MountRecursiveReadOnlyUnverified { target })
                }
            }
        }
    }
}

fn compare_host_config(
    plan: &AgentEnvironmentPlan,
    facts: &ContainerEnvironmentFacts,
    diff: &mut EnvironmentDiff,
) {
    let expected_network = plan.network().as_docker_value();
    if facts.host.network_mode.as_deref() != Some(expected_network) {
        diff.push(EnvironmentDiffEntry::NetworkMismatch {
            expected: expected_network.to_string(),
            actual: facts.host.network_mode.clone(),
        });
    }

    if facts.host.privileged {
        diff.push(EnvironmentDiffEntry::PrivilegedEnabled { actual: true });
    }

    if !facts.host.cap_add.is_empty() {
        diff.push(EnvironmentDiffEntry::CapabilityAdded {
            caps: facts.host.cap_add.clone(),
        });
    }

    let expected_drop = expected_cap_drop(plan);
    if !same_ignoring_case(&expected_drop, &facts.host.cap_drop) {
        diff.push(EnvironmentDiffEntry::CapabilityDropMismatch {
            expected: expected_drop,
            actual: facts.host.cap_drop.clone(),
        });
    }

    if plan.options().no_new_privileges
        && !facts
            .host
            .security_opt
            .iter()
            .any(|option| option == "no-new-privileges")
    {
        diff.push(EnvironmentDiffEntry::SecurityOptionMissing {
            option: "no-new-privileges".to_string(),
        });
    }

    for mount in &facts.mounts {
        if exposes_docker_socket(mount) {
            diff.push(EnvironmentDiffEntry::DockerSocketExposed {
                target: mount.target.clone(),
            });
        }
    }

    let expected_memory = match plan.options().memory.as_deref() {
        Some(value) => match parse_docker_size(value) {
            Some(bytes) => Some(bytes),
            None => {
                diff.push(EnvironmentDiffEntry::PlanInvalid {
                    reason: format!("内存限制 `{value}` 不是可解析的容量"),
                });
                None
            }
        },
        None => None,
    };
    let actual_memory = facts
        .host
        .memory_bytes
        .filter(|value| *value > 0)
        .map(|value| value as u64);
    if expected_memory != actual_memory {
        diff.push(EnvironmentDiffEntry::MemoryLimitMismatch {
            expected: expected_memory,
            actual: facts.host.memory_bytes.filter(|value| *value > 0),
        });
    }

    let expected_nano_cpus = match plan.options().cpus.as_deref() {
        Some(value) => match parse_cpu_count(value) {
            Some(cpus) => Some(cpus),
            None => {
                diff.push(EnvironmentDiffEntry::PlanInvalid {
                    reason: format!("CPU 限制 `{value}` 不是可解析的数值"),
                });
                None
            }
        },
        None => None,
    };
    let actual_nano_cpus = facts.host.nano_cpus.filter(|value| *value > 0);
    if expected_nano_cpus != actual_nano_cpus {
        diff.push(EnvironmentDiffEntry::CpuLimitMismatch {
            expected: expected_nano_cpus,
            actual: actual_nano_cpus,
        });
    }

    let expected_pids = plan.options().pids_limit.map(i64::from);
    let actual_pids = facts.host.pids_limit.filter(|value| *value > 0);
    if expected_pids != actual_pids {
        diff.push(EnvironmentDiffEntry::PidsLimitMismatch {
            expected: expected_pids,
            actual: actual_pids,
        });
    }
}

fn expected_cap_drop(plan: &AgentEnvironmentPlan) -> Vec<String> {
    if plan.options().cap_drop_all {
        vec!["ALL".to_string()]
    } else {
        Vec::new()
    }
}

fn same_ignoring_case(expected: &[String], actual: &[String]) -> bool {
    expected.len() == actual.len()
        && expected.iter().all(|expected| {
            actual
                .iter()
                .any(|actual| actual.eq_ignore_ascii_case(expected))
        })
}

fn exposes_docker_socket(mount: &ContainerMountFacts) -> bool {
    mount.source.ends_with("/docker.sock") || mount.target == "/var/run/docker.sock"
}

/// 解析 Docker 容量字符串（`8g`、`512m`、`1024`）。
pub fn parse_docker_size(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (digits, multiplier) = match value.chars().last()?.to_ascii_lowercase() {
        'b' => {
            let body = value[..value.len() - 1].to_ascii_lowercase();
            match body.chars().last() {
                Some('k') => (&value[..value.len() - 2], 1024_u64),
                Some('m') => (&value[..value.len() - 2], 1024 * 1024),
                Some('g') => (&value[..value.len() - 2], 1024 * 1024 * 1024),
                _ => (value, 1),
            }
        }
        'k' => (&value[..value.len() - 1], 1024),
        'm' => (&value[..value.len() - 1], 1024 * 1024),
        'g' => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    let amount = digits.trim().parse::<u64>().ok()?;
    amount.checked_mul(multiplier)
}

/// 解析 Docker CPU 数值（`4`、`1.5`、`500m`）为 `NanoCpus`。
pub fn parse_cpu_count(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (digits, scale) = match value.chars().last()? {
        'm' | 'M' => (&value[..value.len() - 1], 0.001_f64),
        _ => (value, 1.0_f64),
    };
    let amount = digits.trim().parse::<f64>().ok()?;
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    let nanos = amount * scale * 1_000_000_000.0;
    if nanos > i64::MAX as f64 {
        return None;
    }
    Some(nanos.round() as i64)
}

#[cfg(test)]
mod tests {
    use crate::ContainerMountType;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::*;
    use crate::environment::plan::WorkspaceMountPlan;
    use crate::inspect::ContainerHostFacts;
    use crate::mount::ReadOnlyBindMount;

    const GENERATION: &str = "8f058410-c69e-45d6-a10d-d204db72b30a";

    fn generation() -> Uuid {
        Uuid::parse_str(GENERATION).expect("uuid")
    }

    fn reference_mount(alias: &str) -> ContainerMount {
        ContainerMount::ReadOnlyBind(
            ReadOnlyBindMount::for_reference("/srv/mai-references/linux/repo", alias)
                .expect("reference mount"),
        )
    }

    fn plan(mounts: Vec<ContainerMount>) -> AgentEnvironmentPlan {
        AgentEnvironmentPlan::builder(
            "agent-1",
            "ghcr.io/example/agent:latest",
            WorkspaceMountPlan::new("mai-team-workspace-agent-1", None::<String>)
                .expect("workspace"),
        )
        .effective_image_id("sha256:effective")
        .generation(generation())
        .mounts(mounts)
        .build()
        .expect("plan")
    }

    fn host_facts() -> ContainerHostFacts {
        ContainerHostFacts {
            network_mode: Some("host".to_string()),
            privileged: false,
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            memory_bytes: Some(8589934592),
            nano_cpus: Some(4000000000),
            pids_limit: Some(2048),
            security_opt: Vec::new(),
        }
    }

    fn workspace_mount_fact() -> ContainerMountFacts {
        ContainerMountFacts {
            target: "/workspace".to_string(),
            mount_type: ContainerMountType::Volume,
            source: "mai-team-workspace-agent-1".to_string(),
            read_only: false,
            propagation: None,
            recursive_read_only: None,
            volume_name: Some("mai-team-workspace-agent-1".to_string()),
            volume_subpath: None,
        }
    }

    fn reference_mount_fact(alias: &str) -> ContainerMountFacts {
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

    fn facts(mounts: Vec<ContainerMountFacts>) -> ContainerEnvironmentFacts {
        ContainerEnvironmentFacts {
            id: "abc123".to_string(),
            name: "mai-team-agent-1-g8f058410c69e".to_string(),
            state: "running".to_string(),
            running: true,
            image_reference: "ghcr.io/example/agent:latest".to_string(),
            image_id: "sha256:effective".to_string(),
            agent_id: Some("agent-1".to_string()),
            project_id: Some("project-1".to_string()),
            generation: Some(generation()),
            sidecar: false,
            sidecar_kind: None,
            mounts,
            host: host_facts(),
        }
    }

    #[test]
    fn matching_environment_has_no_differences() {
        let plan = plan(vec![reference_mount("linux")]);
        let facts = facts(vec![workspace_mount_fact(), reference_mount_fact("linux")]);

        let diff = compare_environment(&plan, &facts);

        assert!(diff.is_empty(), "unexpected diff: {diff}");
        assert_eq!(diff.to_string(), "no differences");
    }

    #[test]
    fn empty_mount_plan_requires_reconfiguration_when_actual_mounts_remain() {
        let plan = plan(Vec::new());
        let facts = facts(vec![workspace_mount_fact(), reference_mount_fact("linux")]);

        let diff = compare_environment(&plan, &facts);

        assert!(!diff.is_empty(), "清空引用后不得因为 mounts 为空而判定复用");
        assert_eq!(
            diff.entries(),
            &[EnvironmentDiffEntry::UnexpectedMount {
                target: "/references/linux".to_string(),
                mount_type: ContainerMountType::Bind,
            }]
        );
    }

    #[test]
    fn workspace_only_environment_matches_when_no_references_are_expected() {
        let plan = plan(Vec::new());
        let facts = facts(vec![workspace_mount_fact()]);

        assert!(compare_environment(&plan, &facts).is_empty());
    }

    #[test]
    fn reports_mount_access_propagation_and_recursive_regressions() {
        let plan = plan(vec![reference_mount("linux")]);
        let mut mutable = reference_mount_fact("linux");
        mutable.read_only = false;
        mutable.propagation = Some("shared".to_string());
        mutable.recursive_read_only = Some(false);

        let diff = compare_environment(&plan, &facts(vec![workspace_mount_fact(), mutable]));

        assert_eq!(
            diff.entries(),
            &[
                EnvironmentDiffEntry::MountAccessMismatch {
                    target: "/references/linux".to_string(),
                    expected_read_only: true,
                    actual_read_only: false,
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
            ]
        );
    }

    #[test]
    fn treats_an_unreported_bind_recursive_flag_as_unverified() {
        let plan = plan(vec![reference_mount("linux")]);
        let mut unverified = reference_mount_fact("linux");
        unverified.recursive_read_only = None;

        let diff = compare_environment(&plan, &facts(vec![workspace_mount_fact(), unverified]));

        assert_eq!(
            diff.entries(),
            &[EnvironmentDiffEntry::MountRecursiveReadOnlyUnverified {
                target: "/references/linux".to_string(),
            }]
        );
    }

    #[test]
    fn reports_missing_and_mismatched_reference_mounts() {
        let plan = plan(vec![reference_mount("linux")]);
        let missing = compare_environment(&plan, &facts(vec![workspace_mount_fact()]));
        assert_eq!(
            missing.entries(),
            &[EnvironmentDiffEntry::MissingMount {
                target: "/references/linux".to_string(),
            }]
        );

        let mut other_source = reference_mount_fact("linux");
        other_source.source = "/srv/mai-references/other/repo".to_string();
        let mismatched =
            compare_environment(&plan, &facts(vec![workspace_mount_fact(), other_source]));
        assert_eq!(
            mismatched.entries(),
            &[EnvironmentDiffEntry::MountSourceMismatch {
                target: "/references/linux".to_string(),
                expected: "/srv/mai-references/linux/repo".to_string(),
                actual: "/srv/mai-references/other/repo".to_string(),
            }]
        );

        let mut volume_instead = reference_mount_fact("linux");
        volume_instead.mount_type = ContainerMountType::Volume;
        volume_instead.volume_name = Some("unexpected".to_string());
        let kind = compare_environment(&plan, &facts(vec![workspace_mount_fact(), volume_instead]));
        assert_eq!(
            kind.entries(),
            &[EnvironmentDiffEntry::MountTypeMismatch {
                target: "/references/linux".to_string(),
                expected: ContainerMountType::Bind,
                actual: ContainerMountType::Volume,
            }]
        );
    }

    #[test]
    fn reports_image_generation_network_and_privilege_differences() {
        let plan = plan(Vec::new());
        let mut actual = facts(vec![workspace_mount_fact()]);
        actual.image_id = "sha256:other".to_string();
        actual.image_reference = "ghcr.io/example/other:latest".to_string();
        actual.generation = None;
        actual.host.network_mode = Some("bridge".to_string());
        actual.host.privileged = true;
        actual.host.cap_add = vec!["SYS_ADMIN".to_string()];
        actual.host.cap_drop = vec!["NET_RAW".to_string()];
        actual.host.memory_bytes = Some(1073741824);
        actual.host.nano_cpus = Some(1000000000);
        actual.host.pids_limit = Some(100);

        let diff = compare_environment(&plan, &actual);
        let entries = diff.entries();

        assert!(entries.contains(&EnvironmentDiffEntry::GenerationMismatch {
            expected: Some(generation()),
            actual: None,
        }));
        assert!(entries.contains(&EnvironmentDiffEntry::ImageMismatch {
            expected: "sha256:effective".to_string(),
            actual: "sha256:other".to_string(),
        }));
        assert!(
            entries.contains(&EnvironmentDiffEntry::ImageReferenceMismatch {
                expected: "ghcr.io/example/agent:latest".to_string(),
                actual: "ghcr.io/example/other:latest".to_string(),
            })
        );
        assert!(entries.contains(&EnvironmentDiffEntry::NetworkMismatch {
            expected: "host".to_string(),
            actual: Some("bridge".to_string()),
        }));
        assert!(entries.contains(&EnvironmentDiffEntry::PrivilegedEnabled { actual: true }));
        assert!(entries.contains(&EnvironmentDiffEntry::CapabilityAdded {
            caps: vec!["SYS_ADMIN".to_string()],
        }));
        assert!(
            entries.contains(&EnvironmentDiffEntry::CapabilityDropMismatch {
                expected: Vec::new(),
                actual: vec!["NET_RAW".to_string()],
            })
        );
        assert!(
            entries.contains(&EnvironmentDiffEntry::MemoryLimitMismatch {
                expected: Some(8589934592),
                actual: Some(1073741824),
            })
        );
        assert!(entries.contains(&EnvironmentDiffEntry::CpuLimitMismatch {
            expected: Some(4000000000),
            actual: Some(1000000000),
        }));
        assert!(entries.contains(&EnvironmentDiffEntry::PidsLimitMismatch {
            expected: Some(2048),
            actual: Some(100),
        }));
    }

    #[test]
    fn reports_agent_mismatch_sidecar_and_docker_socket() {
        let plan = plan(Vec::new());
        let mut actual = facts(vec![workspace_mount_fact()]);
        actual.agent_id = None;
        actual.sidecar = true;
        actual.sidecar_kind = Some("agent-mcp".to_string());
        actual.mounts.push(ContainerMountFacts {
            target: "/var/run/docker.sock".to_string(),
            mount_type: ContainerMountType::Bind,
            source: "/var/run/docker.sock".to_string(),
            read_only: false,
            propagation: None,
            recursive_read_only: None,
            volume_name: None,
            volume_subpath: None,
        });

        let diff = compare_environment(&plan, &actual);
        let entries = diff.entries();

        assert!(entries.contains(&EnvironmentDiffEntry::AgentMismatch {
            expected: "agent-1".to_string(),
            actual: None,
        }));
        assert!(entries.contains(&EnvironmentDiffEntry::UnexpectedSidecar {
            kind: Some("agent-mcp".to_string()),
        }));
        assert!(
            entries.contains(&EnvironmentDiffEntry::DockerSocketExposed {
                target: "/var/run/docker.sock".to_string(),
            })
        );
    }

    #[test]
    fn required_mounts_only_check_the_given_targets() {
        let expected = vec![reference_mount("linux")];
        let mut actual = facts(vec![workspace_mount_fact(), reference_mount_fact("linux")]);
        actual.mounts.push(ContainerMountFacts {
            target: "/project/repo".to_string(),
            mount_type: ContainerMountType::Volume,
            source: "review-volume".to_string(),
            read_only: true,
            propagation: None,
            recursive_read_only: None,
            volume_name: Some("review-volume".to_string()),
            volume_subpath: Some("review-contexts/run-1/repo".to_string()),
        });

        assert!(compare_required_mounts(&expected, &actual).is_empty());

        let mut writable = actual.clone();
        writable.mounts[1].read_only = false;
        assert_eq!(
            compare_required_mounts(&expected, &writable).entries(),
            &[EnvironmentDiffEntry::MountAccessMismatch {
                target: "/references/linux".to_string(),
                expected_read_only: true,
                actual_read_only: false,
            }]
        );

        let empty = facts(vec![workspace_mount_fact()]);
        assert_eq!(
            compare_required_mounts(&expected, &empty).entries(),
            &[EnvironmentDiffEntry::MissingMount {
                target: "/references/linux".to_string(),
            }]
        );
    }

    #[test]
    fn parses_docker_size_and_cpu_values() {
        assert_eq!(parse_docker_size("8g"), Some(8589934592));
        assert_eq!(parse_docker_size("512m"), Some(536870912));
        assert_eq!(parse_docker_size("1024"), Some(1024));
        assert_eq!(parse_docker_size("2gb"), Some(2147483648));
        assert_eq!(parse_docker_size(""), None);
        assert_eq!(parse_docker_size("lots"), None);

        assert_eq!(parse_cpu_count("4"), Some(4000000000));
        assert_eq!(parse_cpu_count("1.5"), Some(1500000000));
        assert_eq!(parse_cpu_count("500m"), Some(500000000));
        assert_eq!(parse_cpu_count(""), None);
        assert_eq!(parse_cpu_count("-1"), None);
    }
}
