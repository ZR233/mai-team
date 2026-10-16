//! 崩溃恢复与失败态判定（T31/T32）：注入标记、收据终态与旧容器保留。

use super::*;

/// T31：崩溃注入之后必须按收据恢复；未注入时给 `blocked`。
pub(super) fn crash_recovery(probe: &Probe, input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T31.crash_recovery_from_receipt");
    // 注入进程在写标记后立刻硬退出，因此崩溃轮的证据行不存在——判据是标记文件本身。
    let markers = injected_markers(&probe.config.evidence_dir);
    if markers.is_empty() {
        outcome.block("没有找到任何注入标记（需要 apply/recover --inject-phase <phase> 后重跑）");
    }
    for marker in &markers {
        outcome.pass(format!("注入标记存在：{}", marker.display()));
    }
    if !markers.is_empty() && !input.commands.contains("recover") {
        outcome.block("注入之后还没有执行过 recover：恢复结论必须来自恢复命令");
    }
    let active = input
        .facts
        .iter()
        .flat_map(|agent| {
            agent
                .operations
                .iter()
                .filter(|operation| operation.active())
                .map(|operation| {
                    format!(
                        "{}:{}（{}）",
                        agent.agent_key, operation.id, operation.phase
                    )
                })
        })
        .collect::<Vec<_>>();
    if active.is_empty() {
        outcome.pass("注入阶段的所有收据都已进入终态，恢复有明确结论");
    } else {
        outcome.block(format!("恢复仍在进行：{active:?}"));
    }
    let failed = input
        .facts
        .iter()
        .flat_map(|agent| {
            agent
                .operations
                .iter()
                .filter(|operation| operation.phase == ReferenceApplyPhase::Failed)
                .map(|operation| format!("{}:{}", agent.agent_key, operation.id))
        })
        .collect::<Vec<_>>();
    if failed.is_empty() {
        outcome.pass("恢复过程没有留下 failed 收据");
    } else {
        outcome.fail(format!("存在 failed 收据，不得当作恢复成功：{failed:?}"));
    }
    outcome.finish("崩溃注入后按最新收据恢复，无 failed 收据被当作成功")
}

/// T32：失败时必须保留（且停止）旧容器，不退回基础镜像。
pub(super) async fn failure_keeps_old_container(
    probe: &Probe,
    input: &CheckInput<'_>,
) -> Result<Check> {
    let mut outcome = Outcome::new("T32.failure_keeps_old_container");
    let base_image_id = probe.docker.image_id(&probe.config.agent_image).await?;
    let mut samples = 0usize;
    for agent in input.facts {
        let Some(latest) = agent
            .operations
            .iter()
            .rfind(|operation| operation.phase == ReferenceApplyPhase::Failed)
        else {
            continue;
        };
        let converged = agent
            .binding
            .as_ref()
            .is_some_and(|binding| binding.applied_revision == agent.config_revision);
        if converged {
            continue;
        }
        samples += 1;
        let label = &agent.agent_key;
        outcome.pass(format!(
            "{label}：收据 {} 处于 failed（{}）",
            latest.id,
            latest
                .failure
                .clone()
                .unwrap_or_else(|| "无失败详情".to_string())
        ));
        match &latest.old_container_id {
            Some(old) => match probe.docker.inspect_container_environment(old).await {
                Ok(facts) if !facts.running => outcome.pass(format!(
                    "{label}：旧容器 {old} 保留且已停止（state={}）",
                    facts.state
                )),
                Ok(facts) => outcome.fail(format!(
                    "{label}：旧容器 {old} 仍在运行（state={}）",
                    facts.state
                )),
                Err(error) => outcome.fail(format!("{label}：旧容器 {old} 已不存在（{error}）")),
            },
            None => outcome.block(format!("{label}：failed 收据没有旧容器记录")),
        }
        match &agent.container {
            Some(container) if container.image_id == base_image_id => {
                outcome.fail(format!("{label}：当前容器退回基础镜像"))
            }
            Some(_) => outcome.pass(format!("{label}：当前容器没有退回基础镜像")),
            None => outcome.block(format!("{label}：没有当前容器可核对")),
        }
    }
    if samples == 0 {
        outcome.block("当前没有处于失败态且未收敛的收据（需要注入故障后再次运行）");
    }
    Ok(outcome.finish("失败时旧容器保留为 stopped 且不退回基础镜像"))
}

/// 证据目录里已经落盘的崩溃注入标记。
fn injected_markers(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut markers = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.starts_with("injected-") && name.ends_with(".json")
                        })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    markers.sort();
    markers
}
