//! T29 的小型共享工具：统一超时、回执解析与 Profile 常量。

use std::future::Future;
use std::time::Duration;

use mai_protocol::AgentId;
use serde_json::{Value, json};

use crate::error::{DriverError, Result, error};
use crate::host::RuntimeHost;

/// `system-agents/executor` 允许 spawn/close，且是 child 的合法协作 Profile。
pub(super) const EXECUTOR_PROFILE: &str = "executor";

/// provider 在发出工具调用前的真实延迟，给 CAS/apply/delete 留出并发窗口。
pub(super) const PROVIDER_DELAY_SECS: u64 = 8;

/// T29 的单步硬上限。配置更短时采用配置，更长时不超过 120 秒。
pub(super) fn operation_limit(host: &RuntimeHost) -> Duration {
    host.config.operation_timeout.min(Duration::from_secs(120))
}

/// Turn 等待上限；不能超过 root 给定的 turn timeout。
pub(super) fn turn_limit(host: &RuntimeHost) -> Duration {
    host.config.turn_timeout.min(Duration::from_secs(120))
}

/// 给一个真实 Runtime 操作加硬超时；超时本身失败，不重试也不降级。
pub(super) async fn bounded<F, T, E>(host: &RuntimeHost, label: &str, future: F) -> Result<T>
where
    F: Future<Output = std::result::Result<T, E>>,
    DriverError: From<E>,
{
    let limit = operation_limit(host);
    match tokio::time::timeout(limit, future).await {
        Ok(result) => result.map_err(DriverError::from),
        Err(_) => Err(error(
            "t29_operation_timeout",
            format!("T29 操作 {label} 超过 {limit:?} 未返回"),
        )),
    }
}

/// 给一个不会返回 `Result` 的真实投影读取加硬超时。
pub(super) async fn bounded_value<F, T>(host: &RuntimeHost, label: &str, future: F) -> Result<T>
where
    F: Future<Output = T>,
{
    let limit = operation_limit(host);
    match tokio::time::timeout(limit, future).await {
        Ok(value) => Ok(value),
        Err(_) => Err(error(
            "t29_operation_timeout",
            format!("T29 投影操作 {label} 超过 {limit:?} 未返回"),
        )),
    }
}

/// 只从产品 `spawn_agent` 回执解析 agentId，不根据名字或数据库猜测。
pub(super) fn agent_id_from_receipt(receipt: &str) -> Option<AgentId> {
    let parsed: Value = serde_json::from_str(receipt).ok()?;
    parsed
        .get("agentId")
        .and_then(Value::as_str)
        .and_then(|text| AgentId::parse_str(text).ok())
        .or_else(|| parse_embedded_agent_id(&parsed))
}

/// 兼容产品工具输出包一层 result/output/text 的真实形状；不做宽松任意文本搜索。
fn parse_embedded_agent_id(parsed: &Value) -> Option<AgentId> {
    let nested = parsed
        .get("result")
        .or_else(|| parsed.get("output"))
        .or_else(|| parsed.get("text"))?
        .as_str()?;
    let nested: Value = serde_json::from_str(nested).ok()?;
    nested
        .get("agentId")
        .and_then(Value::as_str)
        .and_then(|text| AgentId::parse_str(text).ok())
}

/// 生成一段简短事实，用于并发结果中的原始错误说明。
pub(super) fn result_fact<T>(result: &Result<T>) -> Value {
    match result {
        Ok(_) => json!({ "ok": true }),
        Err(cause) => json!({
            "ok": false,
            "code": cause.code(),
            "message": cause.message(),
        }),
    }
}
