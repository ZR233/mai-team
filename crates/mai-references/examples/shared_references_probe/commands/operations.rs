//! 收据驱动：按最新收据换代、为落后修订重新入队、并发目标序列与 ensure 复用探测。

use super::targets::{TargetBindings, save_alias_bindings, save_targets};
use super::*;

/// T30：G1 应用途中再保存 G2/G3，验证旧目标不能覆盖最新目标且最终收敛。
pub(super) async fn concurrent_target_sequence(probe: &Probe, report: &mut Report) -> Result<()> {
    let linux = probe.config.linux_alias.clone();
    let private = probe.config.private_alias.clone();
    let g1 = save_alias_bindings(probe, std::slice::from_ref(&linux)).await?;
    let receipts = latest_active_receipts(probe).await?;
    let mut tasks = Vec::new();
    for receipt in receipts {
        let applications = Arc::clone(&probe.applications);
        let host = Arc::clone(&probe.host);
        tasks.push(tokio::spawn(async move {
            applications.run_operation(&*host, receipt).await
        }));
    }
    // 不等 G1：立刻保存更新的目标。
    let g2 = save_alias_bindings(probe, &[linux, private]).await?;
    let g3 = save_targets(probe, TargetBindings::MetadataOnly).await?;
    let mut outcomes = Vec::new();
    for task in tasks {
        match task.await {
            Ok(Ok(())) => outcomes.push(json!({ "result": "ok" })),
            Ok(Err(error)) => outcomes.push(json!({
                "result": "error",
                "error_code": error.code().to_string(),
                "error_message": error.message().to_string(),
            })),
            Err(error) => outcomes.push(json!({
                "result": "join_error",
                "error_message": error.to_string(),
            })),
        }
    }
    let final_receipts = probe.store.reference_operations().await?;
    let superseded = final_receipts
        .iter()
        .filter(|receipt| receipt.operation.phase == ReferenceApplyPhase::Superseded)
        .count();
    report.fact(
        "concurrent",
        json!({
            "g1_revision": g1.revision,
            "g2_revision": g2.revision,
            "g3_revision": g3.revision,
            "g1_outcomes": outcomes,
            "superseded_operations": superseded,
        }),
    )?;
    if superseded == 0 {
        report.check(Check::blocked(
            "T30.stale_target_superseded",
            "本轮没有观察到被取代的旧目标（G1 可能在 G2 保存前就已结束；可用 --inject-phase 扩大重叠窗口后重跑）",
        ));
    } else {
        report.check(Check::pass(
            "T30.stale_target_superseded",
            format!("{superseded} 条旧目标收据被标记为 superseded，未覆盖新目标"),
        ));
    }
    Ok(())
}

/// 每 Agent 的最新收据里仍在进行中的那些。
async fn latest_active_receipts(probe: &Probe) -> Result<Vec<PersistedReferenceOperation>> {
    let receipts = probe.store.reference_operations().await?;
    let mut latest: BTreeMap<Uuid, PersistedReferenceOperation> = BTreeMap::new();
    for receipt in receipts {
        latest.insert(receipt.operation.agent_id, receipt);
    }
    Ok(latest
        .into_values()
        .filter(|receipt| {
            !matches!(
                receipt.operation.phase,
                ReferenceApplyPhase::Succeeded
                    | ReferenceApplyPhase::Failed
                    | ReferenceApplyPhase::Superseded
            )
        })
        .collect())
}

/// 按最新收据驱动状态机直到没有进行中的收据；失败如实记录，不做重试循环。
pub(super) async fn run_operations(probe: &Probe, report: &mut Report) -> Result<Vec<Value>> {
    let deadline = tokio::time::Instant::now() + probe.config.operation_timeout;
    let mut log = Vec::new();
    let mut rounds = 0usize;
    loop {
        rounds += 1;
        let pending = latest_active_receipts(probe).await?;
        if pending.is_empty() {
            break;
        }
        if rounds > 64 {
            return err(
                "operation_loop",
                "换代轮次超过 64，拒绝继续（可能出现了自我重排的收据）",
            );
        }
        if tokio::time::Instant::now() > deadline {
            return err(
                "operation_timeout",
                format!(
                    "{} 秒内仍有 {} 条收据未进入终态",
                    probe.config.operation_timeout.as_secs(),
                    pending.len()
                ),
            );
        }
        let mut tasks = tokio::task::JoinSet::new();
        for receipt in pending {
            let applications = Arc::clone(&probe.applications);
            let host = Arc::clone(&probe.host);
            let store = Arc::clone(&probe.store);
            tasks.spawn(async move {
                let operation_id = receipt.operation.id;
                let agent_id = receipt.operation.agent_id;
                let phase_before = receipt.operation.phase;
                let target_revision = receipt.operation.target_revision;
                let outcome = applications.run_operation(&*host, receipt).await;
                let phase_after = store
                    .reference_operations()
                    .await?
                    .into_iter()
                    .find(|candidate| candidate.operation.id == operation_id)
                    .map(|candidate| candidate.operation.phase.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let mut entry = json!({
                    "operation_id": operation_id,
                    "agent_id": agent_id,
                    "target_revision": target_revision,
                    "phase_before": phase_before.to_string(),
                    "phase_after": phase_after,
                });
                match outcome {
                    Ok(()) => {
                        entry["result"] = Value::String("ok".to_string());
                    }
                    Err(error) => {
                        entry["result"] = Value::String("error".to_string());
                        entry["error_code"] = Value::String(error.code().to_string());
                        entry["error_message"] = Value::String(error.message().to_string());
                    }
                }
                Ok::<_, ProbeError>(entry)
            });
        }
        while let Some(result) = tasks.join_next().await {
            log.push(
                result.map_err(|error| {
                    ProbeError::new("operation_join_failed", error.to_string())
                })??,
            );
        }
        report.fact("operations", &log)?;
    }
    Ok(log)
}

/// 恢复路径：把“绑定修订落后于配置修订”的 Agent 重新入队，再交给调用方收敛。
pub(super) async fn enqueue_unmet_targets(probe: &Probe, report: &mut Report) -> Result<()> {
    let mut enqueued = Vec::new();
    let latest = probe
        .store
        .reference_operations()
        .await?
        .into_iter()
        .map(|receipt| (receipt.operation.agent_id, receipt.operation.phase))
        .collect::<BTreeMap<_, _>>();
    for kind in [ProjectKind::A, ProjectKind::B] {
        let project = kind.id();
        let configuration = probe.store.project_reference_configuration(project).await?;
        let inventory = probe.store.reference_agent_inventory(project).await?;
        let mut unmet = Vec::new();
        for (agent_id, physical) in inventory {
            if !physical {
                continue;
            }
            let applied = probe
                .store
                .reference_binding(agent_id)
                .await?
                .map(|binding| binding.applied_revision)
                .unwrap_or(0);
            // recover 是管理员显式重试；同修订 Failed 也必须重新建立意图。
            if applied < configuration.revision
                || latest.get(&agent_id) == Some(&ReferenceApplyPhase::Failed)
            {
                unmet.push(agent_id);
            }
        }
        if unmet.is_empty() {
            continue;
        }
        probe
            .applications
            .request_apply(
                project,
                mai_protocol::ApplyProjectReferencesRequest {
                    expected_revision: configuration.revision,
                    policy: ReferenceApplyPolicy::WhenIdle,
                },
            )
            .await?;
        enqueued.push(json!({
            "project": kind.slug(),
            "revision": configuration.revision,
            "agents": unmet,
        }));
    }
    report.fact("enqueued_applications", &enqueued)?;
    Ok(())
}

/// 快照收据的 JSON 投影（`EnvironmentSnapshotReceipt` 本身不实现 Serialize）。
fn snapshot_receipt_fact(receipt: Option<&mai_docker::EnvironmentSnapshotReceipt>) -> Value {
    match receipt {
        Some(receipt) => json!({
            "operation_id": receipt.operation_id,
            "agent_id": receipt.agent_id,
            "container_id": receipt.container_id,
            "image_name": receipt.image_name,
            "image_id": receipt.image_id,
        }),
        None => Value::Null,
    }
}

/// T15：重复 ensure 必须复用同一容器，且不产生新的 commit（快照收据不变）。
pub(super) async fn reuse_probe(probe: &Probe, report: &mut Report) -> Result<()> {
    let mut outcome = checks::Outcome::new("T15.ensure_reuse_same_container");
    let mut records = Vec::new();
    for agent in probe.fixtures.selected(probe.config.provision) {
        let Some(binding) = probe.store.reference_binding(agent.id()).await? else {
            outcome.block(format!("{}：没有已发布绑定", agent.key));
            continue;
        };
        let facts = probe
            .docker
            .inspect_container_environment(&binding.container_id)
            .await?;
        let snapshot = environment::current_snapshot(probe, agent).await?;
        let plan = mai_references::plan_from_environment(
            agent.id(),
            facts.generation,
            &binding.effective_image_id,
            &snapshot,
            &facts,
        )?;
        let before = probe.docker.snapshot_receipt(binding.generation).await?;
        let mut matched = Vec::new();
        let mut problems = Vec::new();
        for round in 1..=2u8 {
            match probe.docker.inspect_environment(&plan).await? {
                mai_docker::ExistingEnvironment::Matching(verified) => {
                    let handle = probe.docker.prepare_verified_container(&verified).await?;
                    matched.push(json!({ "round": round, "container_id": handle.id }));
                }
                mai_docker::ExistingEnvironment::ReconfigurationRequired(diff) => {
                    problems.push(format!("第 {round} 次 ensure 需要重配置：{diff}"))
                }
                mai_docker::ExistingEnvironment::Absent => {
                    problems.push(format!("第 {round} 次 ensure 认为环境不存在"))
                }
            }
        }
        let after = probe.docker.snapshot_receipt(binding.generation).await?;
        let before_fact = snapshot_receipt_fact(before.as_ref());
        let after_fact = snapshot_receipt_fact(after.as_ref());
        let same_container = matched.iter().all(|entry| {
            entry.get("container_id").and_then(Value::as_str) == Some(binding.container_id.as_str())
        });
        let no_new_snapshot = before == after;
        if problems.is_empty() && same_container && no_new_snapshot {
            outcome.pass(format!(
                "{}：两次 ensure 都复用容器 {}，且未产生新的 commit",
                agent.key, binding.container_id
            ));
        } else {
            outcome.fail(format!(
                "{}：{}（复用同一容器={}，快照未变={}）",
                agent.key,
                problems.join("；"),
                same_container,
                no_new_snapshot
            ));
        }
        records.push(json!({
            "agent_key": agent.key,
            "binding_container_id": binding.container_id,
            "generation": binding.generation,
            "ensure_rounds": matched,
            "snapshot_before": before_fact,
            "snapshot_after": after_fact,
            "same_container": same_container,
            "no_new_snapshot": no_new_snapshot,
        }));
    }
    report.fact("reuse", &records)?;
    report.check(outcome.finish("重复 ensure 复用同一容器，未产生新的 commit/镜像"));
    Ok(())
}
