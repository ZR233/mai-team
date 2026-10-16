//! 换代保留性判定（T27）：三类数据标记、实际卷名与生效镜像 ID。

use super::*;

/// T27：换代后三类数据保留，卷名与生效镜像 ID 与换代前一致。
pub(super) async fn preservation(probe: &Probe, input: &CheckInput<'_>) -> Result<Check> {
    let mut outcome = Outcome::new("T27.preservation_across_generation");
    let previous_agents = input
        .previous
        .map(crate::evidence::agents_by_key)
        .unwrap_or_default();
    let base_image_id = probe.docker.image_id(&probe.config.agent_image).await?;
    let mut samples = 0usize;
    for agent in input.facts {
        let Some(container) = &agent.container else {
            continue;
        };
        samples += 1;
        let label = &agent.agent_key;
        let expected_markers = [
            (
                MARKER_USR_LOCAL,
                marker_content(&agent.agent_key, "usr-local"),
            ),
            (
                MARKER_WORKSPACE,
                marker_content(&agent.agent_key, "workspace"),
            ),
            (MARKER_ANON, marker_content(&agent.agent_key, "anon")),
        ];
        for (path, expected) in expected_markers {
            match agent
                .markers
                .iter()
                .find(|marker| marker.path == path)
                .map(|marker| (marker.state.as_str(), marker.observed.as_deref()))
            {
                Some(("present", Some(observed))) if observed.trim() == expected.as_str() => {
                    outcome.pass(format!(
                        "{label}：{path} 保留（可写层/工作区/匿名卷标记一致）"
                    ))
                }
                Some((state, observed)) => outcome.fail(format!(
                    "{label}：{path} 未保留（state={state} observed={observed:?}）"
                )),
                None => outcome.block(format!("{label}：没有 {path} 的观察记录")),
            }
        }
        if let Some(previous) = previous_agents.get(label)
            && let Some(before_mounts) = previous
                .pointer("/container/mounts")
                .and_then(Value::as_array)
        {
            for before in before_mounts {
                if before.get("mount_type").and_then(Value::as_str) != Some("volume") {
                    continue;
                }
                let target = before
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = before
                    .get("volume_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match container.mounts.iter().find(|mount| mount.target == target) {
                    Some(actual) => {
                        let actual_name = actual
                            .volume_name
                            .clone()
                            .unwrap_or_else(|| actual.source.clone());
                        if actual_name == name {
                            outcome.pass(format!("{label}：{target} 仍复用同一卷 {name}"));
                        } else {
                            outcome.fail(format!(
                                "{label}：{target} 的卷从 {name} 变成 {actual_name}"
                            ));
                        }
                    }
                    None => {
                        outcome.fail(format!("{label}：换代后缺少卷挂载 {target}（原卷 {name}）"))
                    }
                }
            }
        }
        if let Some(binding) = &agent.binding {
            match probe.docker.snapshot_receipt(binding.generation).await? {
                Some(receipt) => {
                    if receipt.image_id == container.image_id {
                        outcome.pass(format!(
                            "{label}：容器使用收据快照镜像 {}（generation {}）",
                            receipt.image_id, binding.generation
                        ));
                    } else {
                        outcome.fail(format!(
                            "{label}：容器镜像 {} != 收据快照 {}",
                            container.image_id, receipt.image_id
                        ));
                    }
                    if container.image_id != base_image_id {
                        outcome.pass(format!("{label}：没有退回基础镜像"));
                    } else {
                        outcome.fail(format!(
                            "{label}：容器使用基础镜像 {base_image_id}，已安装软件会丢失"
                        ));
                    }
                }
                None if binding.generation == agent.agent_id => {
                    if container.image_id == base_image_id {
                        outcome.pass(format!("{label}：本轮未换代，仍是首次创建的基础镜像"));
                    } else {
                        outcome.block(format!(
                            "{label}：generation 为首次创建，但镜像 {} 不是基础镜像 {}",
                            container.image_id, base_image_id
                        ));
                    }
                }
                None => outcome.fail(format!(
                    "{label}：generation {} 没有可找回的快照镜像收据",
                    binding.generation
                )),
            }
        }
    }
    if samples == 0 {
        outcome.block("没有任何真实容器，无法判定保留性");
    }
    Ok(outcome.finish("换代后三类数据保留，卷名与生效镜像与换代前一致"))
}
