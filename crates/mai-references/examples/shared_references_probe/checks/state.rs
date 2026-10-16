//! 绑定与实际容器一致性、owner 唯一性与目标收敛判定。

use super::*;

/// 已发布绑定必须指向真实存在的同一个容器。
pub(super) fn binding_matches_container(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("env.binding_matches_container");
    for agent in input.facts {
        let Some(binding) = &agent.binding else {
            continue;
        };
        let label = &agent.agent_key;
        match &agent.container {
            Some(container)
                if container.id == binding.container_id
                    && container.generation == Some(binding.generation) =>
            {
                outcome.pass(format!(
                    "{label}：绑定 generation {} 指向真实容器 {}",
                    binding.generation, container.id
                ));
            }
            Some(container) => outcome.fail(format!(
                "{label}：绑定容器 {} / generation {} 与实际容器 {} / {:?} 不一致（lookup={}）",
                binding.container_id,
                binding.generation,
                container.id,
                container.generation,
                agent.container_lookup
            )),
            None => outcome.fail(format!(
                "{label}：绑定声明容器 {}，但该容器不存在（lookup={}）",
                binding.container_id, agent.container_lookup
            )),
        }
    }
    outcome.finish("全部已发布绑定都指向真实存在的同一容器")
}

/// 任意时刻每个 Agent 至多一个 owner：同类容器里最多一个在运行。
pub(super) fn single_owner(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T31.single_owner");
    for agent in input.facts {
        let id = agent.agent_id.to_string();
        let owned = input
            .containers
            .iter()
            .filter(|container| {
                !container.sidecar && container.agent_id.as_deref() == Some(id.as_str())
            })
            .collect::<Vec<_>>();
        if owned.is_empty() {
            continue;
        }
        let running = owned
            .iter()
            .filter(|container| container.state == "running")
            .collect::<Vec<_>>();
        if running.len() > 1 {
            outcome.fail(format!(
                "{}：同时有 {} 个 Agent 容器在运行（{:?}），存在双 owner",
                agent.agent_key,
                running.len(),
                running
                    .iter()
                    .map(|container| container.id.clone())
                    .collect::<Vec<_>>()
            ));
            continue;
        }
        match (&agent.binding, running.first()) {
            (Some(binding), Some(container)) if container.id == binding.container_id => {
                outcome.pass(format!(
                    "{}：唯一 owner 是已发布容器 {}（另有 {} 个 stopped 保留）",
                    agent.agent_key,
                    container.id,
                    owned.len() - running.len()
                ));
            }
            (Some(binding), Some(container)) => outcome.fail(format!(
                "{}：运行中的容器 {} 不是已发布绑定 {}",
                agent.agent_key, container.id, binding.container_id
            )),
            (Some(binding), None) => outcome.block(format!(
                "{}：已发布容器 {} 当前未运行，无法判定 owner",
                agent.agent_key, binding.container_id
            )),
            (None, Some(container)) => outcome.pass(format!(
                "{}：只有一个运行中的容器 {}",
                agent.agent_key, container.id
            )),
            (None, None) => outcome.block(format!("{}：受管容器都不在运行", agent.agent_key)),
        }
    }
    outcome.finish("每个 Agent 至多一个运行中的执行环境（由容器清单与实际身份复核）")
}

/// 状态必须收敛到最新目标：绑定修订 == 配置修订，且实际挂载集合与绑定快照一致。
pub(super) fn converged_to_latest_target(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T30.converged_to_latest_target");
    for agent in input.facts.iter().filter(|agent| agent.container.is_some()) {
        let label = &agent.agent_key;
        if agent.operations.iter().any(|operation| operation.active()) {
            outcome.block(format!(
                "{label}：仍存在进行中的收据 {}",
                agent
                    .operations
                    .iter()
                    .filter(|operation| operation.active())
                    .map(|operation| format!("{}（{}）", operation.id, operation.phase))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            continue;
        }
        let Some(binding) = &agent.binding else {
            outcome.fail(format!("{label}：有实际容器但没有已发布绑定"));
            continue;
        };
        let mut problems = Vec::new();
        if binding.applied_revision != agent.config_revision {
            problems.push(format!(
                "applied_revision {} != 配置修订 {}",
                binding.applied_revision, agent.config_revision
            ));
        }
        let expected = reference_targets_of(&binding.references);
        let actual = reference_targets(agent);
        if expected != actual {
            problems.push(format!("实际参考挂载 {actual:?} != 目标 {expected:?}"));
        }
        if problems.is_empty() {
            outcome.pass(format!(
                "{label}：修订 {} 已应用，目标挂载 {expected:?}",
                binding.applied_revision
            ));
        } else {
            outcome.fail(format!("{label}：{}", problems.join("；")));
        }
    }
    outcome.finish("全部仍有执行环境的 Agent 都收敛到最新目标修订")
}

/// 绑定快照里的参考目标路径集合（排序后比较）。
pub(super) fn reference_targets_of(references: &[AppliedReference]) -> Vec<String> {
    let mut targets = references
        .iter()
        .map(|reference| reference.container_path.clone())
        .collect::<Vec<_>>();
    targets.sort();
    targets
}
