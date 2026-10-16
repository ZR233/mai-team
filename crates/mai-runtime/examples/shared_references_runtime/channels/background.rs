//! 后台 exec → 真实 task receipt → `write_stdin`，以及换代/重启后旧租约必须拒绝。
//!
//! `write_stdin.taskId` 使用 provider receipt 中的 canonical `task:<model_call_id>`；
//! exec 内部的 `safe_identity` 不是模型任务身份。正例先在任务运行中写入 stdin，并用真实
//! `wait/get_tool_task` 等到终态读取回显；只有这个 exact taskId 曾可用，换代后的拒绝才有
//! 租约隔离意义。
//!
//! 本用例是 `channels` 场景的最后一个步骤：它会关闭后重建 Runtime，并把 project A 的
//! private alias 还原回夹具原名。

use std::sync::Arc;

use mai_protocol::{AgentId, ReferenceApplyPolicy};
use serde_json::json;

use crate::applied;
use crate::error::Result;
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::{self, RuntimeHost};
use crate::probe::{self, Probe, ProbeCall};
use crate::runtime_config::DriverConfig;

#[path = "background/stale.rs"]
mod stale;

use super::{outputs, swap, task};
use stale::assert_stale_stdin;

/// 执行后台进程与 stdin 用例。
pub async fn case(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    tag: &str,
    agent: AgentId,
    private_target: &str,
) -> Result<()> {
    // 1) 后台 exec：Turn terminal 时任务仍在运行。命令读到 stdin 后先落一个 /tmp 侧写，
    //    便于在换环境前独立证明 write_stdin 已被真实 shell 读到。
    let marker = format!("CHANNEL-BG-{tag}");
    let stdin_side_effect = format!("/tmp/{marker}-stdin");
    let bg_probe = Probe::new(
        format!("channels-bg-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": format!(
                    "sh -c 'echo {marker}-start; read line; printf \"%s\\n\" \"{marker}-got:$line\" > {stdin_side_effect}; echo {marker}-got:$line; sleep 20; echo {marker}-end'"
                ),
                "timeoutSeconds": 120,
            }),
        )],
    );
    let bg_outcome = probe::run(host, agent, &bg_probe, config.turn_timeout).await?;
    evidence.fact("background.exec", json!(&bg_outcome));
    let Some(call) = bg_outcome.call(0) else {
        evidence.fail(
            "channels.background.exec_receipt",
            format!("probe {} 没有任何工具结论", bg_outcome.probe_id),
        );
        return Ok(());
    };
    let call_id = call.expected_call_id.clone();
    let raw_output = outputs::tool_output_for(&outputs::requests(config), &call_id);
    let task_id = raw_output
        .as_deref()
        .and_then(|output| outputs::running_task_receipt(output, &call_id));
    evidence.fact(
        "background.exec_receipt",
        json!({
            "call_id": call_id,
            "task_id": task_id,
            "raw_output": raw_output,
        }),
    );
    let Some(task_id) = task_id else {
        if raw_output.is_none() {
            evidence.blocked(
                "channels.background.exec_is_background",
                "provider 请求里没有 exec 输出，无法解析 running task receipt",
            );
        } else {
            evidence.fail(
                "channels.background.exec_is_background",
                format!(
                    "exec 输出不是与 call_id 精确匹配的 running task receipt：{}",
                    outputs::first_line(raw_output.as_deref().unwrap_or_default())
                ),
            );
        }
        return Ok(());
    };
    evidence.pass(
        "channels.background.exec_is_background",
        format!("Turn terminal 后收到 exact running receipt：{task_id}"),
    );

    let container_before = host.container_id(agent).await?;
    let Some(container_before_id) = container_before.clone() else {
        evidence.blocked(
            "channels.background.exec_process_visible",
            "Agent 没有已发布容器，无法确认后台进程",
        );
        return Ok(());
    };
    let scan_before = outputs::container_processes(host, &container_before_id, &marker).await?;
    evidence.fact(
        "background.process_before",
        json!({
            "container_id": container_before_id,
            "raw": scan_before.raw,
            "matching": scan_before.matching,
        }),
    );
    if scan_before.supported {
        evidence.check(
            "channels.background.exec_process_visible",
            !scan_before.matching.is_empty(),
            format!(
                "容器内确实存在 cmdline 含 exact marker 的后台进程：{:?}",
                scan_before.matching
            ),
            "容器内找不到该后台进程，write_stdin 正例无法判定",
        );
    } else {
        evidence.blocked(
            "channels.background.exec_process_visible",
            format!("容器内进程扫描不可用：{}", scan_before.raw),
        );
        return Ok(());
    }

    // wait 自身允许超时；这里用 1ms 观察 target 仍是 running，证明 stdin 写入发生在旧任务
    // 运行中，而不是 Turn terminal 误读或任务自然结束后的无关调用。
    let running_wait =
        task::wait_for_task(config, host, agent, &task_id, "background-running", tag, 1).await?;
    evidence.fact("background.running_wait", json!(&running_wait));
    let task_running = running_wait.observed
        && running_wait.invocation_success == Some(true)
        && running_wait.task_id_matches
        && running_wait.tool_is_exec
        && running_wait.status.as_deref() == Some("running")
        && running_wait.timed_out == Some(true);
    evidence.check(
        "channels.background.task_still_running",
        task_running,
        format!("真实 wait 观察 {task_id} 仍为 running 且因短超时返回"),
        format!(
            "write_stdin 前任务不是可判定的 running 状态：observed={} success={:?} status={:?} timedOut={:?}",
            running_wait.observed,
            running_wait.invocation_success,
            running_wait.status,
            running_wait.timed_out
        ),
    );
    if !task_running {
        return Ok(());
    }

    // 2) write_stdin 正例。该工具不等待回显；接受性只看真实 ToolInvocation 成功，
    //    内容随后用 /tmp 侧写和任务终态 full payload 双重证明。
    let stdin_text = format!("channel-stdin-{tag}");
    let write_probe = Probe::new(
        format!("channels-stdin-{tag}"),
        vec![ProbeCall::new(
            "write_stdin",
            json!({
                "taskId": task_id,
                "chars": format!("{stdin_text}\n"),
                "maxOutputChars": 4096,
            }),
        )],
    );
    let write_outcome = probe::run(host, agent, &write_probe, config.turn_timeout).await?;
    evidence.fact("background.write_stdin", json!(&write_outcome));
    let write_call = write_outcome.call(0);
    let write_output = write_call.and_then(|call| {
        outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id)
    });
    let write_accepted = write_call.is_some_and(|call| call.observed && call.success == Some(true));
    evidence.fact(
        "background.write_stdin.output",
        json!({ "accepted": write_accepted, "raw": write_output }),
    );
    evidence.check(
        "channels.background.write_stdin_accepted",
        write_accepted,
        format!("write_stdin 使用 receipt taskId {task_id} 被真实任务接受"),
        format!(
            "write_stdin 未被接受：observed={:?} success={:?} raw={:?}",
            write_call.map(|call| call.observed),
            write_call.map(|call| call.success),
            write_output
        ),
    );
    if !write_accepted {
        return Ok(());
    }

    let side_probe = Probe::new(
        format!("channels-stdin-side-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({ "command": format!("sleep 2; cat {stdin_side_effect}") }),
        )],
    );
    let side_outcome = probe::run(host, agent, &side_probe, config.turn_timeout).await?;
    evidence.fact("background.write_stdin_side_effect", json!(&side_outcome));
    let side_output = side_outcome.call(0).and_then(|call| {
        outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id)
    });
    let expected_echo = format!("{marker}-got:{stdin_text}");
    let side_receipt = side_outcome.call(0).and_then(|call| {
        side_output
            .as_deref()
            .and_then(|raw| outputs::running_task_receipt(raw, &call.expected_call_id))
    });
    let side_task = match side_receipt {
        Some(id) => Some(task::wait_and_query(config, host, agent, &id, "stdin-side", tag).await?),
        None => None,
    };
    evidence.fact("background.write_stdin_side_effect_task", json!(&side_task));
    let side_echo = side_task
        .as_ref()
        .and_then(|task| task.full_exec_payload())
        .and_then(|payload| outputs::output_stream(payload, "stdout"))
        .is_some_and(|stream| stream.content.trim() == expected_echo);
    evidence.check(
        "channels.background.write_stdin_reaches_process",
        side_echo,
        format!("运行中的 shell 已读取 stdin 并产生侧写 {expected_echo}"),
        format!(
            "stdin 侧写不符合预期：raw={:?}",
            side_output
                .as_deref()
                .map(|text| text.chars().take(240).collect::<String>())
        ),
    );
    if !side_echo {
        return Ok(());
    }

    // 3) 等待原任务终态并读取 full payload。write_stdin 的即时输出不承担回显语义。
    let terminal =
        task::wait_and_query(config, host, agent, &task_id, "background-final", tag).await?;
    evidence.fact("background.exec_task_terminal", json!(&terminal));
    let payload = terminal.full_exec_payload();
    let Some(payload) = payload else {
        evidence.fail(
            "channels.background.exec_result",
            format!(
                "无法读取后台任务 full payload：status={:?} query_error={:?}",
                terminal.status(),
                terminal.query.parse_error
            ),
        );
        return Ok(());
    };
    let stdout = outputs::output_stream(payload, "stdout");
    let full_echo = stdout
        .as_ref()
        .map(|stream| stream.content.contains(&expected_echo))
        .unwrap_or(false);
    let paths = outputs::exec_capture_paths(config, payload);
    evidence.fact(
        "background.exec_result.paths",
        json!({
            "status": terminal.status(),
            "success_exit_code": outputs::success_exit_code(payload),
            "stdout": &stdout,
            "capture_paths": &paths,
        }),
    );
    evidence.check(
        "channels.background.exec_result",
        terminal.status() == Some("succeeded")
            && outputs::success_exit_code(payload) == Some(0)
            && full_echo,
        format!("后台任务以退出码 0 结束，full stdout 包含 {expected_echo}"),
        format!(
            "后台任务终态不符：status={:?} exit={:?} echo={full_echo}",
            terminal.status(),
            outputs::success_exit_code(payload)
        ),
    );

    // 4) 换代：把 private alias 改成 <private>-channel 并等待真正 applied。
    let project = ProjectKind::A;
    let original = fixtures::project_configuration(&host.store, project).await?;
    // 用另一个真实长任务验证换代撤销运行中的旧租约。
    let lease_probe = Probe::new(
        format!("channels-old-lease-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({"command": format!("sh -c 'while :; do sleep 1; done' {marker}-lease"), "timeoutSeconds": 600}),
        )],
    );
    let lease_outcome = probe::run(host, agent, &lease_probe, config.turn_timeout).await?;
    let lease_task = lease_outcome.call(0).and_then(|call| {
        outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id)
            .and_then(|output| outputs::running_task_receipt(&output, &call.expected_call_id))
    });
    let Some(task_id) = lease_task else {
        evidence.fail(
            "channels.background.active_lease_receipt",
            "换代前长任务没有真实 running 回执",
        );
        return Ok(());
    };
    let active =
        task::wait_for_task(config, host, agent, &task_id, "lease-before-swap", tag, 1).await?;
    let running = active.observed
        && active.invocation_success == Some(true)
        && active.task_id_matches
        && active.tool_is_exec
        && active.status.as_deref() == Some("running")
        && active.timed_out == Some(true);
    evidence.fact("background.active_lease_before_swap", json!(&active));
    evidence.check(
        "channels.background.active_lease_before_swap",
        running,
        "真实 wait 证明旧租约任务在换代前仍运行",
        "旧租约任务在换代前已经结束或无法确认",
    );
    if !running {
        return Ok(());
    }
    let channel_alias = format!("{}-channel", fixtures::ALIAS_PRIVATE);
    let swap_result = swap::replace_and_apply(
        host,
        project,
        original.revision,
        swap::renamed(&original, fixtures::ALIAS_PRIVATE, &channel_alias),
        ReferenceApplyPolicy::WhenIdle,
        config.operation_timeout,
    )
    .await;
    match &swap_result {
        Ok(saved) => {
            let status = host.reference_status(project).await?;
            evidence.fact(
                "background.swap",
                json!({
                    "revision": saved.revision,
                    "status": swap::summarize(&status),
                    "channel_alias": channel_alias,
                }),
            );
            evidence.pass(
                "channels.background.swap_applied",
                format!(
                    "参考配置换代到修订 {} 且已 applied（{}）",
                    saved.revision,
                    swap::summarize(&status)
                ),
            );
        }
        Err(cause) => evidence.fail(
            "channels.background.swap_applied",
            format!("换代未收敛：{}: {}", cause.code(), cause.message()),
        ),
    }
    let container_after = host.container_id(agent).await?;
    evidence.fact(
        "background.container_after",
        json!({ "before": &container_before, "after": &container_after }),
    );
    if swap_result.is_ok() {
        evidence.check(
            "channels.background.container_swapped",
            container_after.is_some() && container_after != container_before,
            format!(
                "换代产生新的执行环境：{:?} -> {:?}",
                container_before, container_after
            ),
            format!(
                "换代后容器没有变化（{:?} -> {:?}），无法验证旧租约失效",
                container_before, container_after
            ),
        );
        let old_running = outputs::container_running(host, &container_before_id).await?;
        evidence.check(
            "channels.background.old_container_not_running",
            !old_running,
            format!("旧容器 {container_before_id} 已不再运行（旧执行环境停止）"),
            format!("旧容器 {container_before_id} 仍在运行，存在双执行环境"),
        );
        if let Some(new_id) = &container_after {
            let scan_after = outputs::container_processes(host, new_id, &marker).await?;
            evidence.fact(
                "background.process_after",
                json!({
                    "container_id": new_id,
                    "raw": scan_after.raw,
                    "matching": scan_after.matching,
                }),
            );
            if scan_after.supported {
                evidence.check(
                    "channels.background.no_leftover_process",
                    scan_after.matching.is_empty(),
                    "新环境里没有旧 marker 进程残留",
                    format!("新环境里仍有旧进程：{:?}", scan_after.matching),
                );
            } else {
                evidence.blocked(
                    "channels.background.no_leftover_process",
                    format!("容器内进程扫描不可用：{}", scan_after.raw),
                );
            }
        } else {
            evidence.blocked(
                "channels.background.no_leftover_process",
                "换代后 Agent 没有已发布容器",
            );
        }
        let targets = host.reference_targets(agent).await?;
        evidence.fact(
            "background.reference_targets",
            json!({ "targets": &targets }),
        );
        evidence.check(
            "channels.background.reference_targets_swapped",
            !targets.contains(&private_target.to_string())
                && targets.contains(&fixtures::container_path(&channel_alias)),
            format!(
                "真实挂载已切换为 {}",
                fixtures::container_path(&channel_alias)
            ),
            format!("真实挂载没有按新 alias 切换：{targets:?}"),
        );
    }

    // 5) 换代后旧 taskId 必须失效（不能触达新容器）。
    assert_stale_stdin(config, host, evidence, agent, &task_id, tag, "swap").await?;

    // 6) 关闭并重建 Runtime：旧 taskId 依旧失效，新 exec 仍可用。
    let store = Arc::clone(&host.store);
    host.shutdown().await;
    let resumed = host::start(config, store).await?;
    resumed.configure_models().await?;
    let restarted_container = resumed.container_id(agent).await?;
    evidence.fact(
        "background.restarted",
        json!({ "container_id": restarted_container }),
    );
    assert_stale_stdin(config, &resumed, evidence, agent, &task_id, tag, "restart").await?;

    let alive_probe = Probe::new(
        format!("channels-restart-alive-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({ "command": format!("echo {marker}-alive") }),
        )],
    );
    let alive = probe::run(&resumed, agent, &alive_probe, config.turn_timeout).await?;
    evidence.fact("background.restart_exec", json!(&alive));
    let alive_payload = alive.call(0).and_then(|call| {
        outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id)
    });
    let alive_preview = alive_payload.as_deref().and_then(outputs::exec_preview);
    let alive_ok = alive_preview
        .as_ref()
        .map(|preview| {
            outputs::state_kind(preview).as_deref() == Some("final")
                && outputs::success_exit_code(preview) == Some(0)
        })
        .unwrap_or(false);
    evidence.check(
        "channels.background.restarted_exec_ok",
        alive_ok,
        "重建 Runtime 后新 exec 仍以退出码 0 成功（旧租约失效不是通道坏了）",
        format!(
            "重建 Runtime 后新 exec 未成功：{}",
            outputs::first_line(alive_payload.as_deref().unwrap_or_default())
        ),
    );

    // 7) 还原夹具 alias（CAS + applied），并断言配置已回到原始名字。
    let current = fixtures::project_configuration(&resumed.store, project).await?;
    if current.bindings != original.bindings {
        match swap::replace_and_apply(
            &resumed,
            project,
            current.revision,
            original.bindings.clone(),
            ReferenceApplyPolicy::WhenIdle,
            config.operation_timeout,
        )
        .await
        {
            Ok(saved) => evidence.pass(
                "channels.background.fixture_restore",
                format!(
                    "夹具配置已还原到修订 {}（{}）",
                    saved.revision,
                    fixtures::ALIAS_PRIVATE
                ),
            ),
            Err(cause) => evidence.fail(
                "channels.background.fixture_restore",
                format!(
                    "夹具配置还原失败（需要 root 手工恢复 private alias）：{}: {}",
                    cause.code(),
                    cause.message()
                ),
            ),
        }
    } else {
        evidence.pass(
            "channels.background.fixture_restore",
            "夹具配置本来就是原始 alias，无需还原",
        );
    }
    applied::assert_fixture_restored(evidence, &resumed.store, project, &channel_alias).await?;
    resumed.shutdown().await;
    Ok(())
}
