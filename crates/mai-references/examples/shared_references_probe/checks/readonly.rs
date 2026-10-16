//! 只读写保护与提权探测（T22/T23）：真实容器内尝试 + 宿主内容摘要前后一致。

use super::attempts::{
    AttemptVerdict, effective_sys_admin, find_reference_file, privilege_attempts, write_attempts,
};
use super::sources::source_digest_for;
use super::*;

/// T22：root shell 的全部写入尝试都必须失败，且宿主内容摘要前后一致。
pub(super) async fn readonly_write_attempts(
    probe: &Probe,
    input: &CheckInput<'_>,
) -> Result<Check> {
    let mut outcome = Outcome::new("T22.readonly_write_rejected");
    let mut samples = 0usize;
    for agent in input.facts {
        let Some(container) = &agent.container else {
            continue;
        };
        let Some(binding) = &agent.binding else {
            continue;
        };
        if binding.references.is_empty() {
            continue;
        }
        if !container.running {
            outcome.block(format!(
                "{}：容器 {} 未运行（state={}），无法完成写入尝试",
                agent.agent_key, container.id, container.state
            ));
            continue;
        }
        for reference in &binding.references {
            samples += 1;
            let label = format!("{}:{}", agent.agent_key, reference.alias);
            let Some(file) =
                find_reference_file(probe, &container.id, &reference.container_path).await?
            else {
                outcome.block(format!("{label}：参考目录里没有可用于尝试的普通文件"));
                continue;
            };
            let before = source_digest_for(probe, reference)?;
            let attempts =
                write_attempts(probe, &container.id, &reference.container_path, &file).await?;
            for attempt in &attempts {
                match attempt.verdict() {
                    AttemptVerdict::Rejected => outcome.pass(format!(
                        "{label}：{} 被拒绝（status={}）",
                        attempt.name, attempt.status
                    )),
                    AttemptVerdict::UnexpectedSuccess => outcome.fail(format!(
                        "{label}：{} 竟然成功（status=0），参考原件可能可写",
                        attempt.name
                    )),
                    AttemptVerdict::ToolMissing => outcome.block(format!(
                        "{label}：{} 无法判定——容器内缺少命令（status={}）",
                        attempt.name, attempt.status
                    )),
                }
            }
            let after = source_digest_for(probe, reference)?;
            match (before, after) {
                (Some(before), Some(after)) if before == after => outcome.pass(format!(
                    "{label}：宿主内容摘要前后一致（{after}）"
                )),
                (Some(before), Some(after)) => outcome.fail(format!(
                    "{label}：宿主内容摘要发生变化（{before} -> {after}）"
                )),
                _ => outcome.block(format!(
                    "{label}：本轮未采集该源宿主摘要（--host-digest 范围不含它；跨轮摘要检查仍然生效）"
                )),
            }
        }
    }
    if samples == 0 {
        outcome.block("没有任何 Agent 应用了启用参考源，无法完成只读写入尝试");
    }
    Ok(outcome.finish("参考路径对 root shell 只读，宿主原件摘要未变"))
}

/// T23：无 CAP_SYS_ADMIN/privileged/docker socket，remount/umount 失败且挂载仍只读。
pub(super) async fn privilege_probes(probe: &Probe, input: &CheckInput<'_>) -> Result<Check> {
    let mut outcome = Outcome::new("T23.no_privilege_escalation");
    let mut samples = 0usize;
    for agent in input.facts {
        let Some(container) = &agent.container else {
            continue;
        };
        let label = &agent.agent_key;
        if !container.running {
            outcome.block(format!("{label}：容器未运行，跳过权限探测"));
            continue;
        }
        samples += 1;
        if container.privileged {
            outcome.fail(format!("{label}：容器是 privileged"));
        } else {
            outcome.pass(format!("{label}：容器非 privileged"));
        }
        if container.cap_add.is_empty() {
            outcome.pass(format!("{label}：没有额外 capability 附加"));
        } else {
            outcome.fail(format!("{label}：存在 cap_add {:?}", container.cap_add));
        }
        match effective_sys_admin(probe, &container.id).await? {
            Some(true) => outcome.fail(format!("{label}：进程 capability 里仍有 CAP_SYS_ADMIN")),
            Some(false) => outcome.pass(format!("{label}：CapEff 不含 CAP_SYS_ADMIN")),
            None => outcome.block(format!("{label}：无法读取 /proc/self/status 的 CapEff")),
        }
        for attempt in privilege_attempts(probe, &container.id, agent).await? {
            match attempt.verdict() {
                AttemptVerdict::Rejected => outcome.pass(format!(
                    "{label}：{} 被拒绝（status={}）",
                    attempt.name, attempt.status
                )),
                AttemptVerdict::UnexpectedSuccess => {
                    outcome.fail(format!("{label}：{} 竟然成功", attempt.name))
                }
                AttemptVerdict::ToolMissing => outcome.block(format!(
                    "{label}：{} 无法判定——容器内缺少命令（status={}）",
                    attempt.name, attempt.status
                )),
            }
        }
        for mount in container.mounts.iter().filter(|mount| {
            mount.target == "/references" || mount.target.starts_with("/references/")
        }) {
            if mount.read_only && mount.recursive_read_only == Some(true) {
                outcome.pass(format!("{label}：{} 仍为只读递归只读", mount.target));
            } else {
                outcome.fail(format!(
                    "{label}：{} 的只读状态异常（read_only={} recursive={:?}）",
                    mount.target, mount.read_only, mount.recursive_read_only
                ));
            }
        }
    }
    if samples == 0 {
        outcome.block("没有运行中的容器可做权限探测");
    }
    Ok(outcome.finish("容器无提权能力，remount/umount 与 socket 探测失败，参考挂载保持只读"))
}
