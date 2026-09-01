use mai_protocol::{AgentRole, AgentSummary};
use pl_core::{
    AgentModelConfig, AgentRoleId, ModelRouteConfig, ProviderId, ReasoningEffort,
    ResolvedModelRoute,
};
use pl_protocol::{AgentProfileSnapshot, AgentWorkspaceMode};
use serde_json::json;

use crate::{Result, RuntimeError, agents};

const PROFILE_SOURCE: &str = "mai-system";
const PROFILE_SCHEMA_REVISION: &str = "1";
const ROLES: [AgentRole; 4] = [
    AgentRole::Planner,
    AgentRole::Explorer,
    AgentRole::Executor,
    AgentRole::Reviewer,
];

/// 从产品模型配置生成本次 turn 可冻结的 PL 原生 Agent Profile 目录。
pub(crate) fn snapshots(models: &AgentModelConfig) -> Result<Vec<AgentProfileSnapshot>> {
    ROLES
        .into_iter()
        .map(|role| snapshot(models, role))
        .collect()
}

/// 为产品工作流直接创建的 child 生成与协作工具相同的 Profile 快照。
pub(crate) fn snapshot(models: &AgentModelConfig, role: AgentRole) -> Result<AgentProfileSnapshot> {
    let role_id = role_id(role)?;
    let route = models.resolve(&role_id).map_err(RuntimeError::Model)?;
    let workspace_mode = workspace_mode(role);
    let system_instructions = agents::task_role_system_prompt(role).to_string();
    let content_hash = pl_core::canonical_json_hash(&json!({
        "schemaRevision": PROFILE_SCHEMA_REVISION,
        "profileId": role_id.as_str(),
        "providerId": route.provider_id.as_str(),
        "model": route.model.slug,
        "effort": route.effort.as_ref().map(ReasoningEffort::as_str),
        "systemInstructions": system_instructions,
        "workspaceMode": workspace_mode,
    }));
    let (display_name, description, when_to_use) = profile_copy(role);
    Ok(AgentProfileSnapshot {
        profile_id: role_id.to_string(),
        display_name: display_name.to_string(),
        description: description.to_string(),
        when_to_use: when_to_use.to_string(),
        system_instructions,
        provider_id: route.provider_id.to_string(),
        model: route.model.slug,
        effort: route.effort.map(|effort| effort.as_str().to_string()),
        source: PROFILE_SOURCE.to_string(),
        revision: PROFILE_SCHEMA_REVISION.to_string(),
        content_hash,
        system: true,
        enabled: true,
        workspace_mode,
    })
}

/// 将产品已创建 Agent 的持久模型与提示词冻结成 PL child session Profile。
pub(crate) fn product_snapshot(
    summary: &AgentSummary,
    system_prompt: Option<&str>,
) -> Result<AgentProfileSnapshot> {
    let role = summary.role.unwrap_or_default();
    let role_id = role_id(role)?;
    let workspace_mode = workspace_mode(role);
    let system_instructions = system_prompt
        .unwrap_or_else(|| agents::task_role_system_prompt(role))
        .to_string();
    let content_hash = pl_core::canonical_json_hash(&json!({
        "schemaRevision": PROFILE_SCHEMA_REVISION,
        "profileId": role_id.as_str(),
        "providerId": summary.provider_id,
        "model": summary.model,
        "effort": summary.reasoning_effort,
        "systemInstructions": system_instructions,
        "workspaceMode": workspace_mode,
    }));
    let (display_name, description, when_to_use) = profile_copy(role);
    Ok(AgentProfileSnapshot {
        profile_id: role_id.to_string(),
        display_name: display_name.to_string(),
        description: description.to_string(),
        when_to_use: when_to_use.to_string(),
        system_instructions,
        provider_id: summary.provider_id.clone(),
        model: summary.model.clone(),
        effort: summary.reasoning_effort.clone(),
        source: "mai-product-agent".to_string(),
        revision: PROFILE_SCHEMA_REVISION.to_string(),
        content_hash,
        system: true,
        enabled: true,
        workspace_mode,
    })
}

/// 将 PL 已规范化的相对写目录转换为 child session 的 canonical receipt。
pub(crate) fn workspace_assignment(
    profile: &AgentProfileSnapshot,
    project_root: &str,
    writable_paths: Option<Vec<String>>,
) -> Result<pl_protocol::AgentWorkspaceAssignmentSnapshot> {
    let writable_paths = match profile.workspace_mode {
        AgentWorkspaceMode::Unrestricted => {
            if writable_paths.is_some() {
                return Err(RuntimeError::InvalidInput(
                    "unrestricted Profile cannot receive writablePaths".to_string(),
                ));
            }
            None
        }
        AgentWorkspaceMode::Directory => writable_paths
            .map(|paths| {
                paths
                    .into_iter()
                    .map(|path| absolute_writable_path(project_root, &path))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?,
        AgentWorkspaceMode::Worktree => {
            return Err(RuntimeError::InvalidInput(
                "mai does not advertise worktree Agent Profiles".to_string(),
            ));
        }
    };
    Ok(pl_protocol::AgentWorkspaceAssignmentSnapshot {
        mode: profile.workspace_mode,
        project_root: project_root.to_string(),
        root: project_root.to_string(),
        writable_paths,
        worktree: None,
    })
}

/// 解析一次 turn 的唯一模型路由。child 必须消费 session 中冻结的 Profile。
pub(crate) fn resolve_route(
    models: &AgentModelConfig,
    role: &AgentRoleId,
    profile: Option<&AgentProfileSnapshot>,
) -> Result<ResolvedModelRoute> {
    let Some(profile) = profile else {
        return models.resolve(role).map_err(RuntimeError::Model);
    };
    if profile.profile_id != role.as_str() {
        return Err(RuntimeError::InvalidInput(format!(
            "frozen Agent Profile `{}` does not match child role `{role}`",
            profile.profile_id
        )));
    }
    let provider = ProviderId::new(profile.provider_id.clone()).map_err(RuntimeError::Model)?;
    let mut scoped = models.clone();
    scoped.routes.insert(
        role.clone(),
        ModelRouteConfig {
            provider,
            model: profile.model.clone(),
            effort: profile.effort.clone().map(ReasoningEffort::new),
        },
    );
    scoped.resolve(role).map_err(RuntimeError::Model)
}

fn role_id(role: AgentRole) -> Result<AgentRoleId> {
    AgentRoleId::new(role.to_string()).map_err(RuntimeError::Model)
}

fn workspace_mode(role: AgentRole) -> AgentWorkspaceMode {
    match role {
        AgentRole::Planner | AgentRole::Explorer | AgentRole::Reviewer => {
            AgentWorkspaceMode::Unrestricted
        }
        AgentRole::Executor => AgentWorkspaceMode::Directory,
    }
}

fn profile_copy(role: AgentRole) -> (&'static str, &'static str, &'static str) {
    match role {
        AgentRole::Planner => (
            "Planner",
            "Produces decision-complete implementation plans.",
            "Use for architecture and implementation planning.",
        ),
        AgentRole::Explorer => (
            "Explorer",
            "Investigates code and product context without modifying it.",
            "Use for focused read-only discovery.",
        ),
        AgentRole::Executor => (
            "Executor",
            "Implements a bounded task in the shared project directory.",
            "Use for scoped implementation and verification.",
        ),
        AgentRole::Reviewer => (
            "Reviewer",
            "Reviews changes and reports blocking findings.",
            "Use for independent correctness review.",
        ),
    }
}

fn absolute_writable_path(project_root: &str, path: &str) -> Result<String> {
    if path == "." {
        return Ok(project_root.to_string());
    }
    let relative = std::path::Path::new(path);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(RuntimeError::InvalidInput(format!(
            "invalid writablePaths entry `{path}`"
        )));
    }
    Ok(std::path::Path::new(project_root)
        .join(relative)
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn system_profile_modes_preserve_pl_semantics() {
        assert_eq!(
            ROLES.map(workspace_mode),
            [
                AgentWorkspaceMode::Unrestricted,
                AgentWorkspaceMode::Unrestricted,
                AgentWorkspaceMode::Directory,
                AgentWorkspaceMode::Unrestricted,
            ]
        );
    }

    #[test]
    fn child_route_uses_frozen_profile_after_current_route_changes() {
        let mut config = crate::MaiConfig::default();
        let role = AgentRole::Reviewer;
        let role_id = role_id(role).unwrap();
        let frozen = snapshot(&config.models, role).unwrap();
        let provider_id = config.models.routes[&role_id].provider.clone();
        let replacement = config.models.providers[&provider_id]
            .effective_models()
            .unwrap()
            .into_iter()
            .find(|model| model.slug != frozen.model)
            .expect("preset provides a second model");
        let replacement_effort = replacement.default_effort().map(ReasoningEffort::new);
        config.models.routes.insert(
            role_id.clone(),
            ModelRouteConfig {
                provider: provider_id,
                model: replacement.slug,
                effort: replacement_effort,
            },
        );

        let current = config.models.resolve(&role_id).unwrap();
        let resolved = resolve_route(&config.models, &role_id, Some(&frozen)).unwrap();

        assert_ne!(current.model.slug, frozen.model);
        assert_eq!(resolved.model.slug, frozen.model);
        assert_eq!(
            frozen.system_instructions,
            agents::task_role_system_prompt(role)
        );
    }
}
