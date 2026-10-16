//! Docker 环境核验、创建、可写层保留与定向清理。
//!
//! 这里的每个入口都是显式的：`inspect_environment` 只回答事实，
//! `create_environment` 只创建候选，清理只按精确 ID 执行，不提供“按标签删全部”的换代路径。

use tokio::process::Command;
use uuid::Uuid;

use crate::args::{create_agent_environment_args, create_agent_mcp_sidecar_container_args};
use crate::client::{DockerClient, stderr_or_stdout};
use crate::container::{ContainerHandle, is_missing_container_error};
use crate::environment::compare::compare_environment;
use crate::environment::plan::{AgentEnvironmentPlan, AgentMcpSidecarPlan};
use crate::environment::preserve::preserved_volume_mounts;
use crate::environment::{
    EnvironmentSnapshotReceipt, EnvironmentSnapshotRequest, ExistingEnvironment, VerifiedContainer,
};
use crate::error::{DockerError, Result};
use crate::image::snapshot_image_facts_from_inspect;
use crate::inspect::{
    ContainerEnvironmentFacts, ContainerVerificationFacts,
    container_environment_facts_from_inspect, container_verification_facts_from_inspect,
};
use crate::mount::{ContainerMount, validate_additional_mounts};
use crate::naming::{
    AGENT_LABEL_KEY, AGENT_MCP_SIDECAR_KIND, MANAGED_LABEL, SNAPSHOT_CONTAINER_LABEL_KEY,
    SNAPSHOT_OPERATION_LABEL_KEY, agent_generation_mcp_sidecar_container_name,
    environment_snapshot_image_name,
};
use crate::selection::{
    ContainerRemovalPolicy, exact_container_removal_targets, find_environment_candidate,
};

use crate::snapshot_resources::{SNAPSHOT_ORIGIN_LABEL_KEY, SnapshotOrigin};

mod sidecar;

use sidecar::{sidecar_issue_summary, sidecar_reuse_issues};

impl DockerClient {
    /// CloneFrom 只复制父容器可写层；子 Agent 的卷和参考挂载由自己的最终计划决定。
    pub async fn commit_clone_parent(
        &self,
        agent_id: String,
        generation: Uuid,
        parent_id: &str,
    ) -> Result<String> {
        let image = environment_snapshot_image_name(&agent_id, generation);
        let managed_label = format!("LABEL {MANAGED_LABEL}");
        let agent_label = format!("LABEL {AGENT_LABEL_KEY}={agent_id}");
        let operation_label = format!("LABEL {SNAPSHOT_OPERATION_LABEL_KEY}={generation}");
        let container_label = format!("LABEL {SNAPSHOT_CONTAINER_LABEL_KEY}={parent_id}");
        let origin_label = format!(
            "LABEL {SNAPSHOT_ORIGIN_LABEL_KEY}={}",
            SnapshotOrigin::Clone.label_value()
        );
        let owner_label = format!(
            "LABEL mai.team.snapshot.owner={}",
            self.snapshot_owner
                .map(|owner| owner.to_string())
                .unwrap_or_default()
        );
        let output = Command::new(&self.binary)
            .args(["commit"])
            .args(["--change", managed_label.as_str()])
            .args(["--change", agent_label.as_str()])
            .args(["--change", operation_label.as_str()])
            .args(["--change", container_label.as_str()])
            .args(["--change", origin_label.as_str()])
            .args(["--change", owner_label.as_str()])
            .arg(parent_id)
            .arg(&image)
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        self.image_id(&image).await
    }

    /// 核对某个 Agent 的期望环境与实际 Docker 状态。
    ///
    /// 期望挂载为空同样会核对：只复用挂载与 HostConfig 都符合计划的容器。
    pub async fn inspect_environment(
        &self,
        plan: &AgentEnvironmentPlan,
    ) -> Result<ExistingEnvironment> {
        let containers = self.list_managed_containers().await?;
        let Some(candidate) = find_environment_candidate(
            &containers,
            plan.agent_id(),
            plan.generation(),
            plan.published_container_id(),
        ) else {
            return Ok(ExistingEnvironment::Absent);
        };

        let facts = self.inspect_container_environment(&candidate.id).await?;
        let diff = compare_environment(plan, &facts);
        if diff.is_empty() {
            Ok(ExistingEnvironment::Matching(VerifiedContainer::verified(
                &facts,
            )))
        } else {
            Ok(ExistingEnvironment::ReconfigurationRequired(diff))
        }
    }

    /// 读取单个容器的完整环境事实；容器不存在时返回 `UnknownContainer`。
    pub async fn inspect_container_environment(
        &self,
        container_id: &str,
    ) -> Result<ContainerEnvironmentFacts> {
        self.inspect_container_environment_facts(&[container_id.to_string()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::UnknownContainer(container_id.to_string()))
    }

    /// 创建候选环境：使用计划挂载与需要保留的既有卷，创建后启动并初始化工作区。
    ///
    /// 本方法不宣称环境已通过核验；发布前必须调用 `verify_environment`。
    pub async fn create_environment(
        &self,
        plan: &AgentEnvironmentPlan,
        preserved: &[ContainerMount],
    ) -> Result<ContainerHandle> {
        validate_additional_mounts(preserved)?;
        let mounts = plan.creation_mounts(preserved)?;
        let name = plan.container_name();
        let args = create_agent_environment_args(plan, &mounts);
        let create = Command::new(&self.binary)
            .args(args.iter().map(String::as_str))
            .output()
            .await?;
        if !create.status.success() {
            let message = stderr_or_stdout(&create);
            if is_name_conflict_error(&message) {
                return Err(DockerError::EnvironmentNameConflict(name));
            }
            return Err(DockerError::CommandFailed(message));
        }
        let id = String::from_utf8(create.stdout)?.trim().to_string();

        if let Err(error) = self.start_container(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(error);
        }
        if let Err(error) = self.ensure_workspace(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(error);
        }

        Ok(ContainerHandle {
            id,
            name,
            image: plan.image_reference().to_string(),
        })
    }

    /// 核验候选人：不一致时返回显式差异，由上层决定收敛方式。
    pub async fn verify_environment(
        &self,
        plan: &AgentEnvironmentPlan,
        container_id: &str,
    ) -> Result<VerifiedContainer> {
        let facts = self.inspect_container_environment(container_id).await?;
        let diff = compare_environment(plan, &facts);
        if !diff.is_empty() {
            return Err(DockerError::EnvironmentReconfigurationRequired(diff));
        }
        Ok(VerifiedContainer::verified(&facts))
    }

    /// 把已核验容器准备成可用的 handle（必要时启动并确保 `/workspace` 存在）。
    pub async fn prepare_verified_container(
        &self,
        verified: &VerifiedContainer,
    ) -> Result<ContainerHandle> {
        // 验证凭据固定的是配置身份；排空回调可能在验证后停止同一个容器。
        // Docker start 对运行中容器幂等，准备阶段重新确保运行态，不能复用旧的 liveness。
        self.start_container(verified.id()).await?;
        self.ensure_workspace(verified.id()).await?;
        Ok(ContainerHandle {
            id: verified.id().to_string(),
            name: verified.name().to_string(),
            image: verified.image_reference().to_string(),
        })
    }

    /// 从实际环境推导出换代时需要原样复用的卷挂载。
    pub async fn preserved_environment_volumes(
        &self,
        container_id: &str,
        plan: &AgentEnvironmentPlan,
    ) -> Result<Vec<ContainerMount>> {
        let facts = self.inspect_container_environment(container_id).await?;
        preserved_volume_mounts(&facts, plan)
    }

    /// 保存已停止容器的可写层，返回可持久化、可按 operation 找回的收据。
    ///
    /// 镜像名由 Agent 与 operation UUID 决定（确定性），并带 operation/容器标签，
    /// 因此在 `commit` 成功但服务崩溃时仍能找回同一镜像。
    pub async fn commit_stopped_environment(
        &self,
        request: &EnvironmentSnapshotRequest,
    ) -> Result<EnvironmentSnapshotReceipt> {
        if self.container_is_running(&request.container_id).await? {
            return Err(DockerError::EnvironmentStateMismatch(format!(
                "container {} must be stopped before snapshotting its writable layer",
                request.container_id
            )));
        }

        let image_name = environment_snapshot_image_name(&request.agent_id, request.operation_id);
        let operation_label = format!(
            "LABEL {SNAPSHOT_OPERATION_LABEL_KEY}={}",
            request.operation_id
        );
        let container_label = format!(
            "LABEL {SNAPSHOT_CONTAINER_LABEL_KEY}={}",
            request.container_id
        );
        let agent_label = format!("LABEL {AGENT_LABEL_KEY}={}", request.agent_id);
        let managed_label = format!("LABEL {MANAGED_LABEL}");
        let origin_label = format!(
            "LABEL {SNAPSHOT_ORIGIN_LABEL_KEY}={}",
            SnapshotOrigin::Generation.label_value()
        );

        let owner_label = format!(
            "LABEL mai.team.snapshot.owner={}",
            self.snapshot_owner
                .map(|owner| owner.to_string())
                .unwrap_or_default()
        );
        let commit = Command::new(&self.binary)
            .args(["commit"])
            .args(["--change", managed_label.as_str()])
            .args(["--change", agent_label.as_str()])
            .args(["--change", operation_label.as_str()])
            .args(["--change", container_label.as_str()])
            .args(["--change", origin_label.as_str()])
            .args(["--change", owner_label.as_str()])
            .arg(&request.container_id)
            .arg(&image_name)
            .output()
            .await?;
        if !commit.status.success() {
            let message = stderr_or_stdout(&commit);
            if is_missing_container_error(&message) {
                return Err(DockerError::UnknownContainer(request.container_id.clone()));
            }
            return Err(DockerError::CommandFailed(message));
        }

        let image_id = self.image_id(&image_name).await?;
        Ok(EnvironmentSnapshotReceipt {
            operation_id: request.operation_id,
            agent_id: request.agent_id.clone(),
            container_id: request.container_id.clone(),
            image_name,
            image_id,
        })
    }

    /// 按 operation 找回快照镜像收据；服务重启后不依赖进程内状态。
    pub async fn snapshot_receipt(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<EnvironmentSnapshotReceipt>> {
        let filter = format!("label={SNAPSHOT_OPERATION_LABEL_KEY}={operation_id}");
        let output = Command::new(&self.binary)
            .args(["image", "ls", "--no-trunc", "--quiet", "--filter"])
            .arg(filter.as_str())
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        let Some(image_id) = String::from_utf8(output.stdout)?
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(ToOwned::to_owned)
        else {
            return Ok(None);
        };

        let inspect = Command::new(&self.binary)
            .args(["image", "inspect"])
            .arg(image_id.as_str())
            .output()
            .await?;
        if !inspect.status.success() {
            let message = stderr_or_stdout(&inspect);
            if is_missing_image_error(&message) {
                return Ok(None);
            }
            return Err(DockerError::CommandFailed(message));
        }
        let Some(facts) = snapshot_image_facts_from_inspect(&String::from_utf8(inspect.stdout)?)?
        else {
            return Ok(None);
        };

        Ok(Some(EnvironmentSnapshotReceipt {
            operation_id,
            agent_id: facts.label(AGENT_LABEL_KEY).unwrap_or_default().to_string(),
            container_id: facts
                .label(SNAPSHOT_CONTAINER_LABEL_KEY)
                .unwrap_or_default()
                .to_string(),
            image_name: facts
                .repo_tags
                .first()
                .cloned()
                .unwrap_or_else(|| facts.id.clone()),
            image_id: facts.id,
        }))
    }

    /// 停止容器但不删除：保留旧环境作为失败时的恢复收据。
    pub async fn stop_container(&self, container_id: &str) -> Result<()> {
        let output = Command::new(&self.binary)
            .args(["stop", container_id])
            .output()
            .await?;
        if output.status.success() {
            return Ok(());
        }
        let message = stderr_or_stdout(&output);
        if is_missing_container_error(&message) {
            return Ok(());
        }
        Err(DockerError::CommandFailed(message))
    }

    /// 只按精确 ID 删除容器：未知 ID 一律拒绝，受保护 ID 一律拒绝，且从不删除卷。
    pub async fn remove_containers_exact(
        &self,
        container_ids: &[String],
        protected_container_ids: &[String],
        policy: ContainerRemovalPolicy,
    ) -> Result<Vec<String>> {
        let containers = self.list_managed_containers().await?;
        let targets = exact_container_removal_targets(
            &containers,
            container_ids,
            protected_container_ids,
            policy,
        )?;
        for id in &targets {
            self.delete_container(id).await?;
        }
        Ok(targets)
    }

    /// 确保存在与 Agent 同 generation 的 MCP sidecar。
    ///
    /// sidecar 显式挂载同一份 typed 只读挂载；只有**完整核对通过**的同 generation
    /// sidecar 才会被复用，旧 generation 以及指向旧源容器/多了参考挂载的 sidecar
    /// 都按精确 ID 定向删除后重建。
    pub async fn ensure_agent_mcp_sidecar_environment(
        &self,
        plan: &AgentMcpSidecarPlan,
    ) -> Result<ContainerHandle> {
        let containers = self.list_managed_containers().await?;
        let agent_sidecars = containers
            .iter()
            .filter(|container| {
                container.sidecar
                    && container.sidecar_kind.as_deref() == Some(AGENT_MCP_SIDECAR_KIND)
                    && container.agent_id.as_deref() == Some(plan.agent_id())
            })
            .collect::<Vec<_>>();

        // sidecar 的卷来自 `--volumes-from`，它的身份与挂载都必须真的对应这个源容器；
        // 源容器不存在时既不能核验也不能创建，直接 fail closed。
        let source = self
            .inspect_container_verification_facts(plan.source_container_id())
            .await?;
        let expected_image_id = self.image_id(plan.image()).await?;

        let mut stale = Vec::new();
        if let Some(candidate) = agent_sidecars
            .iter()
            .find(|container| container.generation == Some(plan.generation()))
        {
            match self
                .inspect_container_verification_facts(&candidate.id)
                .await
            {
                Ok(sidecar) => {
                    let issues = sidecar_reuse_issues(plan, &sidecar, &source, &expected_image_id);
                    if issues.is_empty() {
                        return self
                            .prepare_verified_container(&VerifiedContainer::verified(
                                &sidecar.environment,
                            ))
                            .await;
                    }
                    tracing::warn!(
                        sidecar = %candidate.id,
                        reason = %sidecar_issue_summary(&issues),
                        "MCP sidecar 不再符合复用契约，按精确 ID 重建"
                    );
                    stale.push(candidate.id.clone());
                }
                // 候选在核对前消失：等价于没有可复用的 sidecar，按缺失处理而不是失败。
                Err(DockerError::UnknownContainer(_)) => {}
                Err(error) => return Err(error),
            }
        }
        for container in agent_sidecars {
            if container.generation != Some(plan.generation())
                && !stale.iter().any(|id| id == &container.id)
            {
                stale.push(container.id.clone());
            }
        }
        for id in &stale {
            // 只删除本 Agent 旧 generation 的 sidecar，且只按精确 ID。
            self.delete_container(id).await?;
        }

        let name = agent_generation_mcp_sidecar_container_name(plan.agent_id(), plan.generation());
        let args = create_agent_mcp_sidecar_container_args(
            &name,
            plan.agent_id(),
            plan.image(),
            plan.source_container_id(),
            plan.generation(),
            plan.mounts(),
        );
        let create = Command::new(&self.binary)
            .args(args.iter().map(String::as_str))
            .output()
            .await?;
        if !create.status.success() {
            let message = stderr_or_stdout(&create);
            if is_name_conflict_error(&message) {
                return Err(DockerError::EnvironmentNameConflict(name));
            }
            return Err(DockerError::CommandFailed(message));
        }
        let id = String::from_utf8(create.stdout)?.trim().to_string();
        if let Err(error) = self.start_container(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(error);
        }
        if let Err(error) = self.ensure_workspace(&id).await {
            let _ = self.delete_container(&id).await;
            return Err(error);
        }
        Ok(ContainerHandle {
            id,
            name,
            image: plan.image().to_string(),
        })
    }

    /// 读取容器环境事实以及 sidecar 核验所需的标识（容器创建时间、`--volumes-from` 来源标签）。
    pub(crate) async fn inspect_container_verification_facts(
        &self,
        container_id: &str,
    ) -> Result<ContainerVerificationFacts> {
        let output = Command::new(&self.binary)
            .arg("inspect")
            .arg(container_id)
            .output()
            .await?;
        if !output.status.success() {
            let message = stderr_or_stdout(&output);
            if is_missing_container_error(&message) {
                return Err(DockerError::UnknownContainer(container_id.to_string()));
            }
            return Err(DockerError::CommandFailed(message));
        }
        container_verification_facts_from_inspect(&String::from_utf8(output.stdout)?)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::UnknownContainer(container_id.to_string()))
    }

    async fn inspect_container_environment_facts(
        &self,
        ids: &[String],
    ) -> Result<Vec<ContainerEnvironmentFacts>> {
        let inspect = Command::new(&self.binary)
            .arg("inspect")
            .args(ids)
            .output()
            .await?;
        if inspect.status.success() {
            return container_environment_facts_from_inspect(&String::from_utf8(inspect.stdout)?);
        }

        let message = stderr_or_stdout(&inspect);
        if !is_missing_container_error(&message) {
            return Err(DockerError::CommandFailed(message));
        }

        let mut facts = Vec::new();
        for id in ids {
            let inspect = Command::new(&self.binary)
                .arg("inspect")
                .arg(id)
                .output()
                .await?;
            if !inspect.status.success() {
                let message = stderr_or_stdout(&inspect);
                if is_missing_container_error(&message) {
                    continue;
                }
                return Err(DockerError::CommandFailed(message));
            }
            facts.extend(container_environment_facts_from_inspect(
                &String::from_utf8(inspect.stdout)?,
            )?);
        }
        Ok(facts)
    }

    async fn container_is_running(&self, container_id: &str) -> Result<bool> {
        let output = Command::new(&self.binary)
            .args(["inspect", "--format", "{{.State.Running}}", container_id])
            .output()
            .await?;
        if !output.status.success() {
            let message = stderr_or_stdout(&output);
            if is_missing_container_error(&message) {
                return Err(DockerError::UnknownContainer(container_id.to_string()));
            }
            return Err(DockerError::CommandFailed(message));
        }
        Ok(String::from_utf8(output.stdout)?.trim() == "true")
    }

    pub async fn image_id(&self, image: &str) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(["image", "inspect", "--format", "{{.Id}}", image])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        let id = String::from_utf8(output.stdout)?.trim().to_string();
        if id.is_empty() {
            return Err(DockerError::CommandFailed(format!(
                "docker image inspect returned no id for {image}"
            )));
        }
        Ok(id)
    }
}

/// 受管环境的名字冲突必须显式失败，避免把旧容器覆盖成新 generation。
fn is_name_conflict_error(message: &str) -> bool {
    message.contains("is already in use")
        || message.contains("Conflict. The container name")
        || message.contains("name is already in use")
}

fn is_missing_image_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no such image") || message.contains("no such object")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn preparation_restarts_a_container_stopped_after_verification() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("临时目录");
        let script = directory.path().join("docker");
        let running = directory.path().join("running");
        let body = format!(
            "#!/bin/sh\ncase \"$1\" in\n start) touch '{}' ;;\n exec) test -f '{}' || {{ echo 'container stopped' >&2; exit 1; }} ;;\n *) exit 1 ;;\nesac\n",
            running.display(),
            running.display()
        );
        std::fs::write(&script, body).expect("写 Docker 夹具");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("执行权限");
        let docker = DockerClient::new_with_binary("fixture-image", script.to_string_lossy());
        // 凭据采集时运行，排空回调随后停止；它仍然证明配置一致，但不能证明当前存活。
        let verified = VerifiedContainer {
            id: "verified-container".into(),
            name: "fixture".into(),
            image_reference: "fixture-image".into(),
            image_id: "sha256:fixture".into(),
            generation: None,
            state: "running".into(),
            running: true,
        };
        let prepared = docker
            .prepare_verified_container(&verified)
            .await
            .expect("重新启动后才能准备工作区");
        pretty_assertions::assert_eq!(
            serde_json::to_value(prepared).expect("容器投影"),
            serde_json::json!({"id": "verified-container", "name": "fixture", "image": "fixture-image"})
        );
        assert!(running.exists());
    }

    #[test]
    fn recognizes_container_name_conflicts() {
        assert!(is_name_conflict_error(
            "Error response from daemon: Conflict. The container name \"/mai-team-agent-1\" is already in use by container \"abc\"."
        ));
        assert!(is_name_conflict_error(
            "docker: Error response from daemon: container name is already in use."
        ));
        assert!(!is_name_conflict_error("no such image"));
    }
}
