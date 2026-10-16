//! T39：经真实 Runtime 删除新建验收项目，保留共享原件和其他项目的物理绑定。
use super::support::ProjectHandles;
use crate::error::Result;
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use serde_json::json;

pub async fn scenario(
    host: &RuntimeHost,
    project: &ProjectHandles,
    evidence: &mut Evidence,
) -> Result<()> {
    let snapshot = host.store.load_runtime_snapshot(0).await?;
    let own_agents = snapshot
        .agents
        .iter()
        .filter(|agent| agent.summary.project_id == Some(project.project))
        .map(|agent| agent.summary.id)
        .collect::<Vec<_>>();
    let mut others = Vec::new();
    for agent in snapshot
        .agents
        .iter()
        .filter(|agent| agent.summary.project_id != Some(project.project))
    {
        if let Some(binding) = host.store.reference_binding(agent.summary.id).await? {
            others.push((agent.summary.id, binding.container_id, binding.generation));
        }
    }
    let catalog = host.runtime.reference_catalog();
    let sources = catalog.sources().await?;
    let linux = sources
        .iter()
        .find(|source| source.host_path == host.config.linux_source.to_string_lossy());
    if let Some(linux) = linux {
        let refusal = catalog.unregister(linux.id).await;
        evidence.check(
            "t39.referenced_source_refused",
            refusal
                .as_ref()
                .is_err_and(|error| error.code() == mai_protocol::ReferenceErrorCode::SourceInUse),
            "其他项目引用中的 Linux 源注销被拒绝（409）",
            format!("注销结果不符合预期：{refusal:?}"),
        );
    } else {
        evidence.fail("t39.referenced_source_refused", "未找到固定 Linux 源");
    }
    host.runtime.delete_project(project.project).await?;
    let after = host.store.load_runtime_snapshot(0).await?;
    evidence.check(
        "t39.project_and_agents_deleted",
        !after.projects.iter().any(|item| item.id == project.project)
            && !after
                .agents
                .iter()
                .any(|agent| own_agents.contains(&agent.summary.id)),
        "实际项目删除链路已删除项目及其 Agent 元数据",
        "删除后仍存在项目或 Agent 元数据",
    );
    let operations = host.store.reference_operations().await?;
    evidence.check(
        "t39.operations_deleted",
        !operations
            .iter()
            .any(|receipt| receipt.operation.project_id == project.project),
        "自身应用操作收据随项目删除",
        "删除后仍残留自身操作收据",
    );
    let containers = host.docker.list_managed_containers().await?;
    let own_ids = own_agents
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    evidence.check(
        "t39.containers_deleted",
        !containers.iter().any(|container| {
            container.project_id.as_deref() == Some(project.project.to_string().as_str())
                || container
                    .agent_id
                    .as_ref()
                    .is_some_and(|id| own_ids.contains(id))
        }),
        "项目及 Agent 的执行、MCP 与项目 sidecar 容器已清理",
        "仍存在自身受管容器",
    );
    let mut preserved = true;
    for (id, container_id, generation) in &others {
        preserved &= host
            .store
            .reference_binding(*id)
            .await?
            .is_some_and(|binding| {
                binding.container_id == *container_id && binding.generation == *generation
            });
    }
    evidence.check(
        "t39.other_bindings_preserved",
        preserved,
        "其他项目和非项目 Agent 的容器与 generation 身份保持",
        "其他环境的绑定被删除或改变",
    );
    let remaining = catalog.sources().await?;
    evidence.check(
        "t39.immutable_sources_preserved",
        sources.iter().all(|source| {
            std::path::Path::new(&source.host_path).is_dir()
                && remaining.iter().any(|item| item.id == source.id)
        }),
        "共享源码原件和源登记保持",
        "共享源码原件或源登记缺失",
    );
    evidence.fact(
        "t39.cleanup",
        json!({"project_id":project.project,"agent_ids":own_agents,"other_bindings":others}),
    );
    Ok(())
}
