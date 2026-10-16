//! MCP sidecar 契约判定（T26）：可读不可写，旧 generation 的 sidecar 必须已经退出。

use super::attempts::{AttemptVerdict, find_reference_file, write_attempts};
use super::*;

/// T26：sidecar 可读不可写，旧 generation 的 sidecar 必须已经退出。
pub(super) async fn sidecar_contract(probe: &Probe, input: &CheckInput<'_>) -> Result<Check> {
    let mut outcome = Outcome::new("T26.sidecar_readonly_and_stale_exit");
    let mut samples = 0usize;
    for agent in input.facts {
        let Some(binding) = &agent.binding else {
            continue;
        };
        if binding.references.is_empty() {
            continue;
        }
        samples += 1;
        let sidecar = agent
            .sidecars
            .iter()
            .find(|sidecar| sidecar.generation == Some(binding.generation));
        let Some(sidecar) = sidecar else {
            outcome.fail(format!(
                "{}：缺少 generation {} 的 sidecar（当前 {:?}）",
                agent.agent_key,
                binding.generation,
                agent
                    .sidecars
                    .iter()
                    .map(|sidecar| (sidecar.id.clone(), sidecar.generation))
                    .collect::<Vec<_>>()
            ));
            continue;
        };
        for reference in &binding.references {
            let Some(mount) = sidecar
                .mounts
                .iter()
                .find(|mount| mount.target == reference.container_path)
            else {
                outcome.fail(format!(
                    "{}：sidecar {} 缺少 {} 挂载",
                    agent.agent_key, sidecar.id, reference.container_path
                ));
                continue;
            };
            if mount.read_only
                && mount.recursive_read_only == Some(true)
                && mount.source == reference.host_path
            {
                outcome.pass(format!(
                    "{}：sidecar {} 的 {} 只读递归只读",
                    agent.agent_key, sidecar.id, reference.container_path
                ));
            } else {
                outcome.fail(format!(
                    "{}：sidecar {} 的 {} 只读状态异常（source={} read_only={} recursive={:?}）",
                    agent.agent_key,
                    sidecar.id,
                    reference.container_path,
                    mount.source,
                    mount.read_only,
                    mount.recursive_read_only
                ));
            }
            match find_reference_file(probe, &sidecar.id, &reference.container_path).await? {
                Some(file) => {
                    let read = environment::exec(
                        probe,
                        &sidecar.id,
                        &format!("cat {}", shell_quote(&file)),
                    )
                    .await?;
                    if read.status == 0 {
                        outcome.pass(format!("{}：sidecar 可读取 {file}", agent.agent_key));
                    } else {
                        outcome.fail(format!(
                            "{}：sidecar 读 {file} 失败（status={}）",
                            agent.agent_key, read.status
                        ));
                    }
                    let attempts =
                        write_attempts(probe, &sidecar.id, &reference.container_path, &file)
                            .await?;
                    for attempt in attempts.iter().take(2) {
                        match attempt.verdict() {
                            AttemptVerdict::Rejected => outcome.pass(format!(
                                "{}：sidecar 的 {} 被拒绝",
                                agent.agent_key, attempt.name
                            )),
                            AttemptVerdict::UnexpectedSuccess => outcome.fail(format!(
                                "{}：sidecar 的 {} 竟然成功",
                                agent.agent_key, attempt.name
                            )),
                            AttemptVerdict::ToolMissing => outcome.block(format!(
                                "{}：sidecar 的 {} 无法判定——缺少命令",
                                agent.agent_key, attempt.name
                            )),
                        }
                    }
                }
                None => outcome.block(format!(
                    "{}：sidecar 的参考目录里没有可用于读取的普通文件",
                    agent.agent_key
                )),
            }
        }
    }
    if samples == 0 {
        outcome.block("没有 Agent 绑定启用参考源，无法核对 sidecar 契约");
    }
    // 旧 sidecar 必须退出：本轮与历史记录过的旧 generation sidecar 都不能再存在。
    let mut stale = input.host.stale_sidecars.clone();
    if let Some(previous) = input.previous
        && let Some(entries) = previous
            .pointer("/facts/host/stale_sidecars")
            .and_then(Value::as_array)
    {
        stale.extend(
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned),
        );
    }
    stale.sort();
    stale.dedup();
    for id in stale {
        match probe.docker.inspect_container_environment(&id).await {
            Ok(_) => outcome.fail(format!("旧 sidecar {id} 仍然存在")),
            Err(mai_docker::DockerError::UnknownContainer(_)) => {
                outcome.pass(format!("旧 sidecar {id} 已退出"))
            }
            Err(error) => outcome.block(format!("无法确认旧 sidecar {id}：{error}")),
        }
    }
    Ok(outcome.finish("sidecar 对参考路径可读不可写，旧 generation 的 sidecar 已退出"))
}
