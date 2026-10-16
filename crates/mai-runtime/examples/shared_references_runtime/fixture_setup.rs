//! 把领域驱动建立的物理夹具补齐为真实 Runtime 的 Git 前置资源。
//! 只识别固定夹具项目；容器核验成功后才允许从夹具准备失败恢复为 Ready。
use crate::{
    error::{Result, error},
    fixtures::{self, ProjectKind},
    runtime_config::DriverConfig,
};
use mai_docker::DockerClient;
use mai_protocol::{AgentResourceState, GitAccountRequest, GitProvider};
use mai_store::MaiStore;
use std::sync::Arc;

pub async fn prepare(config: &DriverConfig, store: &Arc<MaiStore>) -> Result<()> {
    let account_id = "reference-runtime-fixture";
    store
        .upsert_git_account(GitAccountRequest {
            id: Some(account_id.into()),
            provider: GitProvider::Github,
            label: "仅隔离 Git HTTP 夹具".into(),
            login: Some("mai-acceptance".into()),
            token: Some("fixture-token-not-a-real-secret".into()),
            ..GitAccountRequest::default()
        })
        .await
        .map_err(|cause| error("fixture_account_failed", cause.to_string()))?;
    let snapshot = store
        .load_runtime_snapshot(0)
        .await
        .map_err(|cause| error("fixture_inventory_failed", cause.to_string()))?;
    let docker = DockerClient::new(config.agent_image.clone());
    for mut project in snapshot.projects {
        if ![ProjectKind::A.id(), ProjectKind::B.id()].contains(&project.id) {
            continue;
        }
        project.git_account_id = Some(account_id.into());
        store
            .save_project(&project)
            .await
            .map_err(|cause| error("fixture_project_failed", cause.to_string()))?;
    }
    for mut agent in snapshot.agents.into_iter().map(|agent| agent.summary) {
        if ![
            "a-planner",
            "a-explorer",
            "a-maintainer",
            "a-reviewer",
            "a-child",
            "a-grandchild",
            "b-executor",
            fixtures::AGENT_NON_PROJECT,
        ]
        .map(fixtures::agent_id)
        .contains(&agent.id)
        {
            continue;
        }
        let Some(binding) = store
            .reference_binding(agent.id)
            .await
            .map_err(|cause| error("fixture_binding_failed", cause.to_string()))?
        else {
            continue;
        };
        let facts = docker
            .inspect_container_environment(&binding.container_id)
            .await
            .map_err(|cause| error("fixture_container_failed", cause.to_string()))?;
        let plan = mai_references::plan_from_environment(
            agent.id,
            facts.generation,
            &binding.effective_image_id,
            &binding.snapshot,
            &facts,
        )
        .map_err(|cause| error("fixture_plan_failed", cause.to_string()))?;
        docker
            .verify_environment(&plan, &binding.container_id)
            .await
            .map_err(|cause| error("fixture_verification_failed", cause.to_string()))?;
        // 配套 prepare-runtime-git.py 已提供真实 bare cache 与现存 Agent 的标准工作区卷。
        // 只修复此前缺失前置资源导致的夹具失败，其他产品失败不在这里放行。
        if agent.resource.state == AgentResourceState::Failed
            && agent.resource.error.as_deref().is_some_and(|message| {
                message.starts_with("project agent resource recovery failed")
            })
        {
            agent.resource.state = AgentResourceState::Ready;
            agent.resource.error = None;
            store
                .save_agent(&agent, None)
                .await
                .map_err(|cause| error("fixture_resource_failed", cause.to_string()))?;
        }
    }
    Ok(())
}
