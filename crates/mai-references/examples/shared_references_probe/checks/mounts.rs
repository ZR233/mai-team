//! 挂载集合与可见性：同源共享（T14）、空清除（T16）、仅改说明（T17）、他项目不可见（T21）。

use super::sources::sources_by_role;
use super::*;

/// T14：同项目两个 Agent 引用同一源时挂载必须相同、只读，且源不按 Agent 增长。
pub(super) fn same_source_same_mount(probe: &Probe, input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T14.same_source_same_mount");
    let alias = probe.config.linux_alias.clone();
    let candidates = input
        .facts
        .iter()
        .filter_map(|agent| {
            let container = agent.container.as_ref()?;
            let reference = agent
                .binding
                .as_ref()?
                .references
                .iter()
                .find(|reference| reference.alias == alias)?
                .clone();
            Some((agent, container, reference))
        })
        .collect::<Vec<_>>();
    if candidates.len() < 2 {
        outcome.block(format!(
            "只有 {} 个 Agent 应用了 {alias} 别名，无法比较“同源共享”",
            candidates.len()
        ));
        return outcome.finish("同源挂载一致");
    }
    let (first_agent, first_container, first_reference) = &candidates[0];
    for (agent, container, reference) in candidates.iter().skip(1) {
        let Some(actual) = container
            .mounts
            .iter()
            .find(|mount| mount.target == reference.container_path)
        else {
            outcome.fail(format!(
                "{}：缺少 {} 挂载",
                agent.agent_key, reference.container_path
            ));
            continue;
        };
        if actual.source == first_reference.host_path
            && actual.read_only
            && actual.recursive_read_only == Some(true)
        {
            outcome.pass(format!(
                "{} 与 {} 共享同一宿主目录 {}（只读、递归只读）",
                agent.agent_key, first_agent.agent_key, actual.source
            ));
        } else {
            outcome.fail(format!(
                "{}：{} 的挂载不符（source={} read_only={} recursive={:?}）",
                agent.agent_key,
                reference.container_path,
                actual.source,
                actual.read_only,
                actual.recursive_read_only
            ));
        }
        if first_container.id == container.id {
            outcome.fail(format!(
                "{} 与 {} 共用同一个容器 {}，不是各自独立环境",
                agent.agent_key, first_agent.agent_key, container.id
            ));
        }
    }
    if let Some(previous) = input.previous {
        let previous_sources = sources_by_role(previous);
        if let Some(linux) = input.sources.iter().find(|source| source.role == "linux") {
            match previous_sources
                .get("linux")
                .and_then(|value| value.get("byte_count"))
            {
                Some(Value::Number(before)) if before.as_u64() == Some(linux.byte_count) => outcome
                    .pass(format!(
                        "{alias} 源字节数保持 {}（未按 Agent 复制增长）",
                        linux.byte_count
                    )),
                Some(Value::Number(before)) => outcome.fail(format!(
                    "{alias} 源字节数从 {before} 变为 {}",
                    linux.byte_count
                )),
                _ => outcome.block("上一轮没有同源字节数可比"),
            }
        }
    }
    outcome.finish("同项目多 Agent 共享同一只读挂载，源未被复制")
}

/// T16：清空绑定后旧的真实挂载必须消失，且不能复用旧容器。
pub(super) fn clear_removes_mounts(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T16.clear_removes_mounts");
    let bound = input.facts.iter().any(|agent| {
        agent.binding.as_ref().is_some_and(|binding| {
            binding
                .references
                .iter()
                .any(|reference| reference.container_path.starts_with("/references/"))
        })
    });
    if bound {
        outcome.block("当前仍有启用绑定，未到“空清除”判定条件");
        return outcome.finish("空清除后旧挂载消失且未复用旧容器");
    }
    let previous_agents = input
        .previous
        .map(crate::evidence::agents_by_key)
        .unwrap_or_default();
    if input.facts.iter().all(|agent| agent.container.is_none()) {
        outcome.block("没有任何真实容器，无法判定空清除");
        return outcome.finish("空清除后旧挂载消失且未复用旧容器");
    }
    for agent in input.facts.iter().filter(|agent| agent.container.is_some()) {
        let label = &agent.agent_key;
        let remaining = reference_targets(agent);
        if remaining.is_empty() {
            outcome.pass(format!("{label}：已没有任何 /references 挂载"));
        } else {
            outcome.fail(format!("{label}：清空后仍存在参考挂载 {remaining:?}"));
        }
        if let Some(before) = previous_agents.get(label) {
            let before_container = before
                .pointer("/binding/container_id")
                .and_then(Value::as_str);
            let before_had_references = before
                .pointer("/binding/references")
                .and_then(Value::as_array)
                .is_some_and(|references| !references.is_empty());
            let now = agent
                .container
                .as_ref()
                .map(|container| container.id.clone());
            if before_had_references
                && before_container.is_some()
                && before_container == now.as_deref()
            {
                outcome.fail(format!(
                    "{label}：清空后仍复用绑定时代的容器 {before_container:?}"
                ));
            } else if before_had_references {
                outcome.pass(format!(
                    "{label}：换代到新容器（旧 {:?} -> 新 {:?}）",
                    before_container, now
                ));
            } else {
                outcome.block(format!("{label}：上一轮没有“引用中”的容器可比"));
            }
        }
    }
    outcome.finish("清空绑定后旧挂载消失且未复用旧容器")
}

/// T17：仅改说明不换代，但模型可见清单必须更新。
pub(super) fn metadata_only_keeps_container(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T17.metadata_only_keeps_container");
    let previous_agents = input
        .previous
        .map(crate::evidence::agents_by_key)
        .unwrap_or_default();
    let mut observed = 0usize;
    for agent in input.facts {
        let (Some(binding), Some(previous)) =
            (&agent.binding, previous_agents.get(&agent.agent_key))
        else {
            continue;
        };
        let (Some(before_fingerprint), Some(before_container), Some(before_manifest)) = (
            previous
                .pointer("/binding/fingerprint")
                .and_then(Value::as_str),
            previous
                .pointer("/binding/container_id")
                .and_then(Value::as_str),
            previous
                .pointer("/model_manifest")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        ) else {
            continue;
        };
        if before_fingerprint != binding.fingerprint {
            continue;
        }
        if Some(before_manifest.as_str()) == agent.model_manifest.as_deref() {
            continue;
        }
        observed += 1;
        let now_container = agent
            .container
            .as_ref()
            .map(|container| container.id.clone());
        if now_container.as_deref() == Some(before_container) {
            outcome.pass(format!(
                "{}：指纹不变而说明变化时容器保持 {}（generation {}）",
                agent.agent_key, before_container, binding.generation
            ));
        } else {
            outcome.fail(format!(
                "{}：仅说明变化却换掉了容器（{} -> {:?}）",
                agent.agent_key, before_container, now_container
            ));
        }
    }
    if observed == 0 {
        outcome.block(format!(
            "未观察到“仅说明/排列变化”的轮次（需要 apply --scenario=metadata_only；已执行场景 {:?}）",
            input.scenarios
        ));
    }
    outcome.finish("仅说明变化时容器 ID 与 generation 不变，模型可见清单更新")
}

/// T21：B 项目与非项目 Agent 既没有 A 私有夹具的挂载，也读不到它。
pub(super) async fn other_agents_cannot_read_private(
    probe: &Probe,
    input: &CheckInput<'_>,
) -> Result<Check> {
    let mut outcome = Outcome::new("T21.other_agents_have_no_private_mount");
    let alias = probe.config.private_alias.clone();
    let target = format!("/references/{alias}");
    let private_path = probe.config.fixture_repo.to_string_lossy().into_owned();
    let mut samples = 0usize;
    for agent in input
        .facts
        .iter()
        .filter(|agent| agent.project_slug.as_deref() != Some("project-a"))
    {
        let label = &agent.agent_key;
        let Some(container) = &agent.container else {
            outcome.block(format!("{label}：没有真实容器可探测"));
            continue;
        };
        samples += 1;
        let mounted = container
            .mounts
            .iter()
            .any(|mount| mount.target == target || mount.source == private_path);
        if mounted {
            outcome.fail(format!("{label}：存在 A 私有夹具挂载 {target}"));
            continue;
        }
        if !container.running {
            outcome.block(format!("{label}：容器未运行，无法在容器内验证读取失败"));
            continue;
        }
        let output = environment::exec(
            probe,
            &container.id,
            &format!("cat {}/README.md", shell_quote(&target)),
        )
        .await?;
        if output.status != 0 {
            outcome.pass(format!(
                "{label}：没有 {target} 挂载，容器内读取也失败（status={}）",
                output.status
            ));
        } else {
            outcome.fail(format!(
                "{label}：虽然无挂载，容器内仍读到了 {target}/README.md"
            ));
        }
    }
    if samples == 0 {
        outcome.block("没有非 A 项目 Agent 的真实容器");
    }
    Ok(outcome.finish("B 项目与非项目 Agent 无 A 私有夹具挂载，且容器内读不到"))
}
