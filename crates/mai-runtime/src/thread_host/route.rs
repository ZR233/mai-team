//! 从 mai 产品事实解析一个 Thread 的唯一模型路由。
//!
//! 每个产品 Agent 在创建时就冻结了自己的 provider/model/effort；mai 继续独占配置与产品状态，
//! core 只接收解析完成、通过 PL 校验的 [`ResolvedModelRoute`]。这里不广播任何全局角色路由，
//! 也不为缺失的 provider 猜测 fallback：解析失败会以显式的不可用结果返回，由装配入口决定如何
//! 处理。

use mai_protocol::AgentSummary;
use pl_model::config::{
    AgentModelConfig, AgentRoleId, ModelRouteConfig, ProviderId, ReasoningEffort,
    ResolvedModelRoute,
};

/// 一个产品 Thread 的模型路由解析结果。
#[derive(Debug)]
pub(crate) enum ProductRoute {
    /// 当前配置下可以建立模型会话。
    Available(Box<ResolvedModelRoute>),
    /// 该 Agent 冻结的 provider/model 在当前配置中不可用；不切换到其它 provider 或模型。
    Unavailable {
        provider_id: String,
        model: String,
        reason: String,
    },
}

/// 解析一个产品 Agent 的模型路由。
///
/// 以 [`AgentSummary`] 中冻结的 `provider_id`/`model`/`reasoning_effort` 为准，在 pl-model 原生
/// 的 [`AgentModelConfig`] 上插入该角色的一次性 route 后完成全部 provider、model 与
/// reasoning 校验；无效身份、缺失 provider/model 都返回 [`ProductRoute::Unavailable`]，不报部分
/// 成功的占位路由。
pub(crate) fn resolve_agent_route(
    models: &AgentModelConfig,
    summary: &AgentSummary,
) -> ProductRoute {
    let unavailable = |reason: String| ProductRoute::Unavailable {
        provider_id: summary.provider_id.clone(),
        model: summary.model.clone(),
        reason,
    };

    let role = summary.role.unwrap_or_default();
    let role_id = match AgentRoleId::new(role.to_string()) {
        Ok(role_id) => role_id,
        Err(error) => return unavailable(error.to_string()),
    };
    let provider = match ProviderId::new(summary.provider_id.clone()) {
        Ok(provider) => provider,
        Err(error) => return unavailable(error.to_string()),
    };

    let mut scoped = models.clone();
    scoped.routes.insert(
        role_id.clone(),
        ModelRouteConfig {
            provider,
            model: summary.model.clone(),
            effort: summary.reasoning_effort.clone().map(ReasoningEffort::new),
        },
    );
    match scoped.resolve(&role_id) {
        Ok(route) => ProductRoute::Available(Box::new(route)),
        Err(error) => unavailable(error.to_string()),
    }
}
