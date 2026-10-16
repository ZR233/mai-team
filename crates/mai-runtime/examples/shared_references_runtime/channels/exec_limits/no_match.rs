//! rg 无匹配用例：区分 exited/exit_code=1 与其它失败。

use mai_protocol::AgentId;
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe::{self, Probe, ProbeCall};
use crate::runtime_config::DriverConfig;

use super::super::{outputs, task};
use super::record_outcome;

pub(super) async fn assert_rg_no_match(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    tag: &str,
    agent: AgentId,
    private_target: &str,
) -> Result<()> {
    let probe_id = format!("channels-rg-{tag}");
    let marker = format!("channel-absent-marker-{tag}");
    let probe = Probe::new(
        probe_id.clone(),
        vec![ProbeCall::new(
            "exec",
            json!({
                "command": format!(
                    "rg --no-config -n -F -- '{marker}' {private_target}"
                )
            }),
        )],
    );
    let outcome = probe::run(host, agent, &probe, config.turn_timeout).await?;
    record_outcome(evidence, "exec.rg_no_match", &outcome);
    let call = outcome.call(0);
    let raw_output = call.and_then(|call| {
        outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id)
    });
    let preview = raw_output.as_deref().and_then(outputs::exec_preview);
    let receipt_task_id = call.and_then(|call| {
        raw_output
            .as_deref()
            .and_then(|output| outputs::running_task_receipt(output, &call.expected_call_id))
    });
    evidence.fact(
        "exec.rg_no_match.provider_view",
        json!({
            "raw_output": raw_output,
            "preview": preview,
            "receipt_task_id": receipt_task_id,
        }),
    );
    if let Some(task_id) = receipt_task_id {
        let facts = task::wait_and_query(config, host, agent, &task_id, "rg-no-match", tag).await?;
        evidence.fact("exec.rg_no_match.task", json!(&facts));
        assert_no_match_payload(evidence, facts.full_exec_payload());
    } else if let Some(preview) = preview {
        assert_no_match_payload(evidence, Some(&preview));
    } else if raw_output.is_none() {
        evidence.blocked(
            "channels.exec.rg_no_match_exit_1",
            "provider 请求里没有该 exec call 的输出，无法判定退出码",
        );
    } else {
        let invocation_failed = call.is_some_and(|call| call.success == Some(false));
        let exact_exit_text = raw_output
            .as_deref()
            .is_some_and(|output| output.contains("Command exited with code 1"));
        evidence.check(
            "channels.exec.rg_no_match_exit_1",
            invocation_failed && exact_exit_text,
            "provider 回填的精确错误文本证明 rg 以退出码 1 结束",
            format!(
                "rg 无匹配输出不是可判定的退出码 1 契约：success={:?} raw={:?}",
                call.map(|call| call.success),
                raw_output
                    .as_deref()
                    .map(|text| text.chars().take(240).collect::<String>())
            ),
        );
    }
    Ok(())
}

fn assert_no_match_payload(evidence: &mut Evidence, payload: Option<&serde_json::Value>) {
    let Some(payload) = payload else {
        evidence.fail(
            "channels.exec.rg_no_match_exit_1",
            "没有拿到可解析的 exec full payload/preview，无法证明退出码 1",
        );
        return;
    };
    let state = outputs::state_kind(payload);
    let result = outputs::final_result_kind(payload);
    let failure = outputs::final_failure_kind(payload);
    let exit_code = outputs::failure_exit_code(payload);
    let stdout = outputs::output_stream(payload, "stdout");
    let stdout_empty = stdout
        .as_ref()
        .map(|stream| stream.content.trim().is_empty())
        .unwrap_or_else(|| outputs::stdout_of(payload).trim().is_empty());
    evidence.fact(
        "exec.rg_no_match.payload",
        json!({
            "state_kind": state,
            "result_kind": result,
            "failure_kind": failure,
            "exit_code": exit_code,
            "stdout_empty": stdout_empty,
        }),
    );
    evidence.check(
        "channels.exec.rg_no_match_exit_1",
        state.as_deref() == Some("final")
            && result.as_deref() == Some("failed")
            && failure.as_deref() == Some("exited")
            && exit_code == Some(1)
            && stdout_empty,
        "rg 无匹配被区分为 failed/exited/exit_code=1 且 stdout 为空",
        format!(
            "rg 无匹配不符合契约：state={state:?} result={result:?} failure={failure:?} exit={exit_code:?} stdout_empty={stdout_empty}"
        ),
    );
}
