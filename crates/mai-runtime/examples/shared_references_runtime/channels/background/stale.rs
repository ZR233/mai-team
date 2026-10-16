//! 旧任务租约失效的精确错误判定。

use mai_protocol::AgentId;
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe::{self, Probe, ProbeCall};
use crate::runtime_config::DriverConfig;

use super::outputs;

pub(super) async fn assert_stale_stdin(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    agent: AgentId,
    task_id: &str,
    tag: &str,
    phase: &str,
) -> Result<Option<String>> {
    let probe = Probe::new(
        format!("channels-stale-{phase}-{tag}"),
        vec![ProbeCall::new(
            "write_stdin",
            json!({ "taskId": task_id, "chars": "stale\n" }),
        )],
    );
    let outcome = probe::run(host, agent, &probe, config.turn_timeout).await?;
    evidence.fact(format!("background.stale_stdin.{phase}"), json!(&outcome));
    let Some(call) = outcome.call(0) else {
        evidence.fail(
            format!("channels.background.stale_stdin_rejected.{phase}"),
            format!("probe {} 没有任何工具结论", outcome.probe_id),
        );
        return Ok(None);
    };
    let output = outputs::tool_output_for(&outputs::requests(config), &call.expected_call_id);
    let text = output.clone().unwrap_or_default();
    let task_not_found = text.contains(&format!("taskId {task_id} was not found in this Thread"));
    let not_running = text.contains(&format!("stdin task {task_id} is not a running exec task"));
    let rejected = task_not_found || not_running;
    evidence.fact(
        format!("background.stale_stdin.{phase}.output"),
        json!({
            "expected_task_id": task_id,
            "raw": output,
            "task_not_found": task_not_found,
            "not_running": not_running,
            "rejected": rejected,
        }),
    );
    if output.is_none() {
        evidence.blocked(
            format!("channels.background.stale_stdin_rejected.{phase}"),
            "provider 请求里没有该 write_stdin 的输出，无法判定旧租约是否失效",
        );
    } else {
        evidence.check(
            &format!("channels.background.stale_stdin_rejected.{phase}"),
            rejected,
            format!(
                "{phase} 后旧 taskId 的 write_stdin 被精确拒绝：{}",
                outputs::first_line(&text)
            ),
            format!(
                "{phase} 后旧 taskId 的 write_stdin 没有返回精确拒绝契约（可能出现跨代 stdin）：{}",
                outputs::first_line(&text)
            ),
        );
    }
    Ok(output)
}
