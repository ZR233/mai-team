//! `tools` 场景：参考源读放行、写拒绝、非 A 项目隔离，全部经**真实注册工具**执行。
//!
//! 判定不依赖容器 Mounts：每个断言都要求真实工具 trace 出现并按序对齐；容器事实与宿主
//! 内容摘要只作为补充证据。已知接口缺口（例如模型可见文件工具没有 `stat_path`/`read_bytes`）
//! 记为 boundary，不伪装成通过。

use mai_protocol::AgentResourceState;
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::fixtures::{self, ProjectKind};
use crate::host::{self, RuntimeHost};
use crate::probe::{self, PROBE_DONE_MARKER, Probe, ProbeCall, ProbeOutcome};
use crate::runtime_config::DriverConfig;

/// 工作区根（与夹具标记 `/workspace/repo/acceptance-marker.txt` 一致）。
const WORKSPACE_ROOT: &str = "/workspace/repo";

/// `tools` 命令入口。
pub async fn run(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    let store = host::open_store(config).await?;
    let runtime = host::start(config, store).await?;
    evidence.fact("provider", runtime.configure_models().await?);
    let outcome = scenario(config, &runtime, evidence).await;
    runtime.shutdown().await;
    outcome
}

async fn scenario(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
) -> Result<()> {
    let tag = probe::run_tag();
    let catalog = host.runtime.reference_catalog();
    evidence.fact(
        "project_b_configuration",
        json!(fixtures::project_configuration(&host.store, ProjectKind::B).await?),
    );
    let project_a = fixtures::project_configuration(&host.store, ProjectKind::A).await?;
    evidence.fact("project_a_configuration", json!(&project_a));
    if let Err(cause) = fixtures::ensure_project_a_contract(&project_a) {
        evidence.blocked("tools.project_a_contract", cause.message());
    } else {
        evidence.pass(
            "tools.project_a_contract",
            "project A 绑定 linux + private-fixture 且都启用",
        );
    }
    let linux = fixtures::source_for_alias(&catalog, &project_a, fixtures::ALIAS_LINUX).await?;
    let private = fixtures::source_for_alias(&catalog, &project_a, fixtures::ALIAS_PRIVATE).await?;
    evidence.fact("registered_sources", json!(host.reference_sources().await?));
    evidence.fact(
        "sources",
        json!({
            "linux": { "id": linux.id, "state": linux.state, "host_path": &linux.host_path, "digest": &linux.manifest_digest },
            "private_fixture": { "id": private.id, "state": private.state, "host_path": &private.host_path, "digest": &private.manifest_digest },
        }),
    );
    evidence.check(
        "tools.sources_ready",
        linux.state.is_ready() && private.state.is_ready(),
        "两个参考源都是 ready，可用于真实挂载",
        format!(
            "参考源未 ready：linux={} private={}",
            linux.state, private.state
        ),
    );

    let maintainer = fixtures::agent_id(fixtures::AGENT_MAINTAINER);
    let summary = host.agent(maintainer).await?;
    evidence.check(
        "tools.parent_ready",
        summary.resource.state == AgentResourceState::Ready,
        format!(
            "{} 的产品资源是 Ready，可接受真实 turn（container={:?}）",
            summary.name, summary.container_id
        ),
        format!(
            "{} 的资源状态是 {:?}，真实 turn 会被拒绝",
            summary.name, summary.resource.state
        ),
    );
    let targets = host.reference_targets(maintainer).await?;
    let linux_target = fixtures::container_path(fixtures::ALIAS_LINUX);
    let private_target = fixtures::container_path(fixtures::ALIAS_PRIVATE);
    evidence.fact("reference_targets", json!(targets));
    evidence.check(
        "tools.mounts_present",
        targets.contains(&linux_target) && targets.contains(&private_target),
        format!("真实容器挂载包含 {linux_target} 与 {private_target}"),
        format!("真实容器挂载缺少参考路径：{targets:?}"),
    );

    let digest_before = host::host_source_digests(config, evidence);

    // 工具发现有 sole_call 约束，单独执行；真实读取随后独立执行。
    let discovery = Probe::new(
        format!("tools-discovery-{tag}"),
        vec![ProbeCall::new(
            "discover_tools",
            json!({ "query": "read file workspace" }),
        )],
    );
    let discovered = probe::run(host, maintainer, &discovery, config.turn_timeout).await?;
    evidence.fact("probe.discovery", json!(&discovered));
    crate::tool_inventory::record(config, evidence, &discovered);

    let read = Probe::new(
        format!("tools-read-{tag}"),
        vec![
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{private_target}/README.md") }),
            ),
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{private_target}/src/fixture.c") }),
            ),
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{linux_target}/Makefile") }),
            ),
            ProbeCall::new(
                "list_files",
                json!({ "path": &private_target, "includeDirs": true }),
            ),
        ],
    );
    let read_outcome = probe::run(host, maintainer, &read, config.turn_timeout).await?;
    evidence.fact("probe.read", json!(&read_outcome));
    assert_calls(
        evidence,
        "tools.read",
        &read_outcome,
        &[
            ("read_file(private README.md)", true),
            ("read_file(private src/fixture.c)", true),
            ("read_file(linux Makefile)", true),
            ("list_files(private)", true),
        ],
    );

    // 2) apply_patch 写 /references 必须失败（新建文件，与上下文无关，失败只能来自授权）。
    let denied_path = format!("{private_target}/runtime-write-probe.txt");
    let patch = Probe::new(
        format!("tools-deny-patch-{tag}"),
        vec![ProbeCall::new(
            "apply_patch",
            json!({
                "input": format!(
                    "*** Begin Patch\n*** Add File: {denied_path}\n+probe write attempt\n*** End Patch"
                )
            }),
        )],
    );
    let patch_outcome = probe::run(host, maintainer, &patch, config.turn_timeout).await?;
    evidence.fact("probe.deny_patch", json!(&patch_outcome));
    assert_calls(
        evidence,
        "tools.apply_patch_reference_denied",
        &patch_outcome,
        &[("apply_patch(/references)", false)],
    );

    // 3) exec 里的 root shell 写参考源必须失败（真实命令，逐条给证据）。
    let exec = Probe::new(
        format!("tools-deny-exec-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": format!(
                    "sh -c 'printf probe >> {private_target}/README.md'; echo rc=$?; touch {private_target}/runtime-exec-probe.txt; echo rc=$?"
                )
            }),
        )],
    );
    let exec_outcome = probe::run(host, maintainer, &exec, config.turn_timeout).await?;
    evidence.fact("probe.deny_exec", json!(&exec_outcome));
    let exec_output = exec_outcome
        .call(0)
        .map(|call| call.output.clone())
        .unwrap_or_default();
    let denied_signal = exec_output.contains("Read-only file system")
        || exec_output.contains("rc=1")
        || exec_output.contains("rc=2");
    evidence.check(
        "tools.exec_reference_write_denied",
        exec_outcome.call(0).is_some_and(|call| call.observed) && denied_signal,
        format!(
            "容器内 root shell 写参考源被拒绝：{}",
            first_line(&exec_output)
        ),
        format!(
            "容器内写参考源没有出现拒绝信号（observed={:?}）：{}",
            exec_outcome.call(0).map(|call| call.observed),
            first_line(&exec_output)
        ),
    );

    // 4) 被拒绝的两次写入都不应产生文件（真实工具读取失败 = 文件不存在）。
    let absent = Probe::new(
        format!("tools-verify-absent-{tag}"),
        vec![
            ProbeCall::new("read_file", json!({ "path": denied_path })),
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{private_target}/runtime-exec-probe.txt") }),
            ),
        ],
    );
    let absent_outcome = probe::run(host, maintainer, &absent, config.turn_timeout).await?;
    evidence.fact("probe.verify_absent", json!(&absent_outcome));
    assert_calls(
        evidence,
        "tools.reference_write_absent",
        &absent_outcome,
        &[
            ("read_file(absent patch target)", false),
            ("read_file(absent exec target)", false),
        ],
    );

    // 5) 工作区内建一个指向参考源的符号链接（真实 exec），随后验证读写表现。
    let link = Probe::new(
        format!("tools-symlink-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": format!("ln -sfn {private_target} {WORKSPACE_ROOT}/runtime-ref-link && echo linked=1")
            }),
        )],
    );
    let link_outcome = probe::run(host, maintainer, &link, config.turn_timeout).await?;
    evidence.fact("probe.symlink", json!(&link_outcome));
    assert_calls(
        evidence,
        "tools.workspace_symlink_created",
        &link_outcome,
        &[("exec(ln -s)", true)],
    );
    let through_link = Probe::new(
        format!("tools-symlink-write-{tag}"),
        vec![
            ProbeCall::new(
                "read_file",
                json!({ "path": format!("{WORKSPACE_ROOT}/runtime-ref-link/README.md") }),
            ),
            ProbeCall::new(
                "apply_patch",
                json!({
                    "input": format!(
                        "*** Begin Patch\n*** Add File: {WORKSPACE_ROOT}/runtime-ref-link/runtime-write-probe.txt\n+probe write attempt\n*** End Patch"
                    )
                }),
            ),
        ],
    );
    let through_link_outcome =
        probe::run(host, maintainer, &through_link, config.turn_timeout).await?;
    evidence.fact("probe.symlink_write", json!(&through_link_outcome));
    assert_calls(
        evidence,
        "tools.workspace_symlink_read_allowed_and_write_denied",
        &through_link_outcome,
        &[
            ("read_file(through symlink)", true),
            ("apply_patch(through symlink)", false),
        ],
    );

    // 6) 正向对照：工作区内的 apply_patch 必须成功（证明工具本身可用）。
    let allow_path = format!("{WORKSPACE_ROOT}/runtime-acceptance/probe-{tag}.txt");
    let allowed = Probe::new(
        format!("tools-workspace-write-{tag}"),
        vec![ProbeCall::new(
            "apply_patch",
            json!({
                "input": format!(
                    "*** Begin Patch\n*** Add File: {allow_path}\n+runtime acceptance {tag}\n*** End Patch"
                )
            }),
        )],
    );
    let allowed_outcome = probe::run(host, maintainer, &allowed, config.turn_timeout).await?;
    evidence.fact("probe.workspace_write", json!(&allowed_outcome));
    assert_calls(
        evidence,
        "tools.workspace_apply_patch_allowed",
        &allowed_outcome,
        &[("apply_patch(workspace)", true)],
    );

    // 7) 非 project A（B 项目 + 非项目 Agent）读 A 私有夹具必须失败。
    for (label, key) in [
        ("project_b", fixtures::AGENT_B_EXECUTOR),
        ("non_project", fixtures::AGENT_NON_PROJECT),
    ] {
        let agent = fixtures::agent_id(key);
        let summary = host.agent(agent).await?;
        evidence.fact(
            format!("{label}_agent"),
            json!({ "key": key, "agent_id": agent, "project_id": summary.project_id, "state": summary.resource.state }),
        );
        let reader = Probe::new(
            format!("tools-{label}-{tag}"),
            vec![
                ProbeCall::new(
                    "read_file",
                    json!({ "path": format!("{private_target}/README.md") }),
                ),
                ProbeCall::new(
                    "read_file",
                    json!({ "path": format!("{linux_target}/Makefile") }),
                ),
            ],
        );
        let outcome = probe::run(host, agent, &reader, config.turn_timeout).await?;
        evidence.fact(format!("probe.{label}"), json!(&outcome));
        assert_calls(
            evidence,
            &format!("tools.{label}_read_denied"),
            &outcome,
            &[
                ("read_file(A private)", false),
                ("read_file(A linux)", false),
            ],
        );
    }

    // 8) 宿主原件摘要前后一致（真实验证器同一套扫描与摘要域）。
    let digest_after = host::host_source_digests(config, evidence);
    for (alias, before) in &digest_before {
        let after = digest_after.get(alias);
        match (before, after) {
            (Some(before), Some(Some(after))) => evidence.check(
                &format!("tools.host_digest_unchanged.{alias}"),
                before == after,
                format!("{alias} 宿主内容摘要前后一致（{after}）"),
                format!("{alias} 宿主内容摘要发生变化：{before} -> {after}"),
            ),
            _ => evidence.blocked(
                format!("tools.host_digest_unchanged.{alias}"),
                format!("本轮没有采到 {alias} 的宿主摘要（部署策略/信任模式不允许扫描）"),
            ),
        }
    }

    // 9) provider 请求证据：确认真实工具输出被回填给模型。
    let requests = probe::provider_requests(&config.provider_evidence_dir);
    if requests.is_empty() {
        evidence.limitation(format!(
            "没有读到 fixture provider 的请求日志（{}）；本场景的判定以真实工具 trace 为准",
            config.provider_evidence_dir.display()
        ));
    } else {
        let with_output = probe::requests_with_tool_output(&requests);
        let finished = requests.iter().any(|request| {
            request
                .to_string()
                .contains(&format!("{PROBE_DONE_MARKER} {tag}"))
        });
        evidence.fact(
            "provider_requests",
            json!({
                "total": requests.len(),
                "with_function_call_output": with_output,
                "with_completion_text": finished,
            }),
        );
        evidence.check(
            "tools.provider_saw_tool_output",
            with_output > 0,
            format!("{with_output} 条 provider 请求回填了 function_call_output（真实工具输出）"),
            "provider 请求里没有 function_call_output，说明工具结果没有回到模型",
        );
        if finished {
            evidence.pass(
                "tools.provider_completion_text",
                format!("provider 历史里出现 {PROBE_DONE_MARKER} {tag}，probe 正常收尾"),
            );
        } else {
            evidence.boundary(
                "tools.provider_completion_text",
                "provider 请求历史里没有出现收尾文本（可能历史被裁剪）；turn 终态与真实工具 trace 已覆盖执行结论",
            );
        }
    }
    Ok(())
}

/// 按序号断言期望的工具调用结果。
fn assert_calls(
    evidence: &mut Evidence,
    prefix: &str,
    outcome: &ProbeOutcome,
    expected: &[(&str, bool)],
) {
    if !outcome.terminal {
        evidence.fail(
            format!("{prefix}.turn_terminal"),
            format!(
                "probe {} 没有进入终态（state={} thread={}）",
                outcome.probe_id, outcome.turn_state, outcome.thread_status
            ),
        );
        return;
    }
    for (index, (label, want_success)) in expected.iter().enumerate() {
        let Some(call) = outcome.call(index) else {
            evidence.fail(
                format!("{prefix}.{label}"),
                format!("probe {} 缺少第 {index} 条调用的结论", outcome.probe_id),
            );
            continue;
        };
        match (call.observed, call.success) {
            (false, _) => evidence.fail(
                format!("{prefix}.{label}"),
                format!(
                    "真实工具 trace 里没有 {}（期望 call_id {}）；probe state={}",
                    call.name, call.expected_call_id, outcome.turn_state
                ),
            ),
            (true, Some(success)) if success == *want_success => evidence.pass(
                format!("{prefix}.{label}"),
                format!(
                    "{} 真实执行 success={success}：{}",
                    call.name,
                    first_line(&call.output_preview)
                ),
            ),
            (true, Some(success)) => evidence.fail(
                format!("{prefix}.{label}"),
                format!(
                    "{} 真实执行 success={success}，期望 {want_success}：{}",
                    call.name,
                    first_line(&call.output_preview)
                ),
            ),
            (true, None) => evidence.fail(
                format!("{prefix}.{label}"),
                format!("{} 有 trace 但没有详情", call.name),
            ),
        }
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.len() > 300 {
        format!("{}…", line.chars().take(300).collect::<String>())
    } else {
        line.to_string()
    }
}
