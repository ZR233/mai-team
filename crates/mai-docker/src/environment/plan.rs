//! 最终环境计划：所有创建路径（新建、项目工作区、审查工作区、CloneFrom、恢复）
//! 共同消费同一份计划，不允许各分支自行决定是否附加参考源码。

use std::collections::HashSet;

use uuid::Uuid;

use crate::args::{ContainerCreateOptions, default_agent_container_options, validate_image};
use crate::error::{DockerError, Result};
use crate::mount::{ContainerMount, ContainerVolumeMount, validate_additional_mounts};
use crate::naming::{
    agent_container_name, agent_generation_container_name, agent_label, agent_workspace_volume,
    generation_label_value,
};

/// workspace 的期望挂载；目标固定为 `/workspace`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMountPlan {
    volume: String,
    volume_subpath: Option<String>,
}

impl WorkspaceMountPlan {
    pub const CONTAINER_TARGET: &'static str = "/workspace";

    pub fn new(
        volume: impl Into<String>,
        volume_subpath: Option<impl Into<String>>,
    ) -> Result<Self> {
        let volume = volume.into();
        let volume_subpath = volume_subpath.map(Into::into);
        if let Some(subpath) = volume_subpath.as_deref() {
            // 复用挂载校验，保证工作区与其它挂载使用同一套目标/子路径规则。
            ContainerVolumeMount::read_only_subpath(
                volume.clone(),
                Self::CONTAINER_TARGET,
                subpath,
            )?;
        } else {
            ContainerVolumeMount::read_write(volume.clone(), Self::CONTAINER_TARGET)?;
        }
        Ok(Self {
            volume,
            volume_subpath,
        })
    }

    /// 未显式指定工作区卷时使用的 Agent 默认卷。
    pub fn agent_default(agent_id: &str) -> Result<Self> {
        Self::new(agent_workspace_volume(agent_id), None::<String>)
    }

    pub fn volume(&self) -> &str {
        &self.volume
    }

    pub fn target(&self) -> &'static str {
        Self::CONTAINER_TARGET
    }

    pub fn volume_subpath(&self) -> Option<&str> {
        self.volume_subpath.as_deref()
    }

    pub fn as_mount(&self) -> Result<ContainerMount> {
        let mount = match self.volume_subpath.as_deref() {
            Some(subpath) => ContainerVolumeMount::read_only_subpath(
                self.volume.clone(),
                Self::CONTAINER_TARGET,
                subpath,
            )?,
            None => ContainerVolumeMount::read_write(self.volume.clone(), Self::CONTAINER_TARGET)?,
        };
        Ok(ContainerMount::Volume(mount))
    }
}

/// 容器的网络模式；计划只接受这里列出的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerNetworkMode {
    Host,
    None,
    Bridge,
}

impl ContainerNetworkMode {
    pub fn as_docker_value(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::None => "none",
            Self::Bridge => "bridge",
        }
    }

    pub fn from_docker_value(value: &str) -> Option<Self> {
        match value {
            "host" => Some(Self::Host),
            "none" => Some(Self::None),
            "bridge" | "default" => Some(Self::Bridge),
            _ => None,
        }
    }
}

/// 一个 Agent 执行环境的最终计划。
///
/// 计划只描述“应该是什么”，不承担授权判断：参考挂载是否被允许由上层策略层决定，
/// 这里只保证参数类型与可比对性。`base_image` 是用户配置的镜像引用，
/// `effective_image_id` 是本次真正使用的镜像 ID（可能是保留可写层后的快照镜像），
/// 两者不能互相替代。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnvironmentPlan {
    agent_id: String,
    project_id: Option<String>,
    generation: Option<Uuid>,
    published_container_id: Option<String>,
    image_reference: String,
    effective_image_id: String,
    workspace: WorkspaceMountPlan,
    additional_mounts: Vec<ContainerMount>,
    options: ContainerCreateOptions,
    network: ContainerNetworkMode,
}

impl AgentEnvironmentPlan {
    pub fn builder(
        agent_id: impl Into<String>,
        image_reference: impl Into<String>,
        workspace: WorkspaceMountPlan,
    ) -> AgentEnvironmentPlanBuilder {
        AgentEnvironmentPlanBuilder {
            agent_id: agent_id.into(),
            image_reference: image_reference.into(),
            workspace,
            project_id: None,
            generation: None,
            published_container_id: None,
            effective_image_id: None,
            additional_mounts: Vec::new(),
            options: default_agent_container_options(),
            network: ContainerNetworkMode::Host,
        }
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    pub fn generation(&self) -> Option<Uuid> {
        self.generation
    }

    /// 已发布绑定记录的容器 ID；只用于定位候选，不是权限证明。
    pub fn published_container_id(&self) -> Option<&str> {
        self.published_container_id.as_deref()
    }

    pub fn image_reference(&self) -> &str {
        &self.image_reference
    }

    pub fn effective_image_id(&self) -> &str {
        &self.effective_image_id
    }

    pub fn workspace(&self) -> &WorkspaceMountPlan {
        &self.workspace
    }

    pub fn additional_mounts(&self) -> &[ContainerMount] {
        &self.additional_mounts
    }

    pub fn options(&self) -> &ContainerCreateOptions {
        &self.options
    }

    pub fn network(&self) -> ContainerNetworkMode {
        self.network
    }

    /// 容器名：带 generation 的计划使用 generation 名字，避免与旧 generation 冲突。
    pub fn container_name(&self) -> String {
        match self.generation {
            Some(generation) => agent_generation_container_name(&self.agent_id, generation),
            None => agent_container_name(&self.agent_id),
        }
    }

    pub(crate) fn agent_label_value(&self) -> String {
        agent_label(&self.agent_id)
    }

    pub(crate) fn generation_label_value(&self) -> Option<String> {
        self.generation.map(generation_label_value)
    }

    /// 期望挂载的集合：工作区 + 计划内附加挂载 + 需要保留的既有卷。
    pub fn creation_mounts(&self, preserved: &[ContainerMount]) -> Result<Vec<ContainerMount>> {
        validate_additional_mounts(preserved)?;
        let mut mounts = Vec::with_capacity(2 + self.additional_mounts.len() + preserved.len());
        mounts.push(self.workspace.as_mount()?);
        mounts.extend(self.additional_mounts.iter().cloned());
        mounts.extend(preserved.iter().cloned());

        let mut targets = HashSet::with_capacity(mounts.len());
        for mount in &mounts {
            if !targets.insert(mount.target().to_string()) {
                return Err(DockerError::InvalidMount(format!(
                    "duplicate mount target `{}`",
                    mount.target()
                )));
            }
        }
        Ok(mounts)
    }
}

/// `AgentEnvironmentPlan` 的命名构造器，避免位置参数与布尔开关。
#[derive(Debug, Clone)]
pub struct AgentEnvironmentPlanBuilder {
    agent_id: String,
    image_reference: String,
    workspace: WorkspaceMountPlan,
    project_id: Option<String>,
    generation: Option<Uuid>,
    published_container_id: Option<String>,
    effective_image_id: Option<String>,
    additional_mounts: Vec<ContainerMount>,
    options: ContainerCreateOptions,
    network: ContainerNetworkMode,
}

impl AgentEnvironmentPlanBuilder {
    pub fn project_id(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    pub fn generation(mut self, generation: Uuid) -> Self {
        self.generation = Some(generation);
        self
    }

    pub fn published_container_id(mut self, container_id: Option<&str>) -> Self {
        self.published_container_id = container_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        self
    }

    pub fn effective_image_id(mut self, image_id: impl Into<String>) -> Self {
        self.effective_image_id = Some(image_id.into());
        self
    }

    pub fn mount(mut self, mount: ContainerMount) -> Self {
        self.additional_mounts.push(mount);
        self
    }

    pub fn mounts(mut self, mounts: impl IntoIterator<Item = ContainerMount>) -> Self {
        self.additional_mounts.extend(mounts);
        self
    }

    pub fn options(mut self, options: ContainerCreateOptions) -> Self {
        self.options = options;
        self
    }

    pub fn network(mut self, network: ContainerNetworkMode) -> Self {
        self.network = network;
        self
    }

    pub fn build(self) -> Result<AgentEnvironmentPlan> {
        validate_agent_id(&self.agent_id)?;
        let image_reference = validate_image(&self.image_reference)?.to_string();
        let effective_image_id = self
            .effective_image_id
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                DockerError::InvalidImage(
                    "environment plan requires the effective image id".to_string(),
                )
            })?;
        if effective_image_id.trim() != effective_image_id {
            return Err(DockerError::InvalidImage(format!(
                "effective image id `{effective_image_id}` must not carry whitespace"
            )));
        }
        validate_additional_mounts(&self.additional_mounts)?;
        self.workspace.as_mount()?;

        Ok(AgentEnvironmentPlan {
            agent_id: self.agent_id,
            project_id: self.project_id,
            generation: self.generation,
            published_container_id: self.published_container_id,
            image_reference,
            effective_image_id,
            workspace: self.workspace,
            additional_mounts: self.additional_mounts,
            options: self.options,
            network: self.network,
        })
    }
}

/// 与 Agent 同 generation 的 MCP sidecar 期望形态。
///
/// sidecar 显式消费同一份 typed 只读挂载，不依赖 `--volumes-from` 携带 bind 挂载，
/// 因此参考路径在 sidecar 内仍然是严格只读。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpSidecarPlan {
    agent_id: String,
    generation: Uuid,
    source_container_id: String,
    image: String,
    mounts: Vec<ContainerMount>,
}

impl AgentMcpSidecarPlan {
    pub fn new(
        agent_id: impl Into<String>,
        generation: Uuid,
        source_container_id: impl Into<String>,
        image: impl Into<String>,
        mounts: Vec<ContainerMount>,
    ) -> Result<Self> {
        let agent_id = agent_id.into();
        validate_agent_id(&agent_id)?;
        let image = validate_image(&image.into())?.to_string();
        let source_container_id = source_container_id.into();
        if source_container_id.trim().is_empty() {
            return Err(DockerError::CommandFailed(
                "mcp sidecar requires the agent container id".to_string(),
            ));
        }
        validate_additional_mounts(&mounts)?;
        Ok(Self {
            agent_id,
            generation,
            source_container_id,
            image,
            mounts,
        })
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn generation(&self) -> Uuid {
        self.generation
    }

    pub fn source_container_id(&self) -> &str {
        &self.source_container_id
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn mounts(&self) -> &[ContainerMount] {
        &self.mounts
    }
}

fn validate_agent_id(agent_id: &str) -> Result<()> {
    if agent_id.trim().is_empty() || agent_id.trim() != agent_id {
        return Err(DockerError::CommandFailed(
            "agent id must not be empty or carry whitespace".to_string(),
        ));
    }
    if agent_id.chars().any(char::is_control) {
        return Err(DockerError::CommandFailed(
            "agent id must not carry control characters".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::mount::ReadOnlyBindMount;

    fn generation() -> Uuid {
        Uuid::parse_str("8f058410-c69e-45d6-a10d-d204db72b30a").expect("uuid")
    }

    fn reference_mount(alias: &str, source: &str) -> ContainerMount {
        ContainerMount::ReadOnlyBind(
            ReadOnlyBindMount::for_reference(source, alias).expect("reference mount"),
        )
    }

    #[test]
    fn plan_uses_generation_container_name_when_present() {
        let workspace = WorkspaceMountPlan::agent_default("agent-1").expect("workspace");
        let plain = AgentEnvironmentPlan::builder("agent-1", "ubuntu:latest", workspace.clone())
            .effective_image_id("sha256:effective")
            .build()
            .expect("plan");
        let generated = AgentEnvironmentPlan::builder("agent-1", "ubuntu:latest", workspace)
            .effective_image_id("sha256:effective")
            .generation(generation())
            .build()
            .expect("plan");

        assert_eq!(plain.container_name(), "mai-team-agent-1");
        assert_eq!(generated.container_name(), "mai-team-agent-1-g8f058410c69e");
        assert_eq!(generated.generation(), Some(generation()));
        assert_eq!(
            generated.generation_label_value().as_deref(),
            Some("mai.team.generation=8f058410-c69e-45d6-a10d-d204db72b30a")
        );
    }

    #[test]
    fn plan_rejects_missing_effective_image_and_overlapping_mounts() {
        let workspace = WorkspaceMountPlan::agent_default("agent-1").expect("workspace");
        assert!(matches!(
            AgentEnvironmentPlan::builder("agent-1", "ubuntu:latest", workspace.clone()).build(),
            Err(DockerError::InvalidImage(_))
        ));
        assert!(matches!(
            AgentEnvironmentPlan::builder("agent-1", "ubuntu:latest", workspace.clone())
                .effective_image_id("sha256:effective")
                .mount(reference_mount("linux", "/srv/mai-references/linux/repo"))
                .mount(reference_mount("linux", "/srv/mai-references/other/repo"))
                .build(),
            Err(DockerError::InvalidMount(_))
        ));
        assert!(matches!(
            WorkspaceMountPlan::new("", None::<String>),
            Err(DockerError::InvalidMount(_))
        ));
    }

    #[test]
    fn creation_mounts_combine_workspace_plan_and_preserved_volumes() {
        let workspace = WorkspaceMountPlan::agent_default("agent-1").expect("workspace");
        let plan = AgentEnvironmentPlan::builder("agent-1", "ubuntu:latest", workspace)
            .effective_image_id("sha256:effective")
            .generation(generation())
            .mount(reference_mount("linux", "/srv/mai-references/linux/repo"))
            .build()
            .expect("plan");
        let preserved = ContainerMount::Volume(
            ContainerVolumeMount::read_write("anonymous-data", "/var/lib/postgresql/data")
                .expect("volume"),
        );

        let mounts = plan.creation_mounts(&[preserved]).expect("creation mounts");
        let targets = mounts
            .iter()
            .map(|mount| mount.target().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            targets,
            vec![
                "/workspace".to_string(),
                "/references/linux".to_string(),
                "/var/lib/postgresql/data".to_string()
            ]
        );
    }

    #[test]
    fn creation_mounts_reject_volume_that_shadows_a_reference_target() {
        let workspace = WorkspaceMountPlan::agent_default("agent-1").expect("workspace");
        let plan = AgentEnvironmentPlan::builder("agent-1", "ubuntu:latest", workspace)
            .effective_image_id("sha256:effective")
            .mount(reference_mount("linux", "/srv/mai-references/linux/repo"))
            .build()
            .expect("plan");
        let shadowing = ContainerMount::Volume(
            ContainerVolumeMount::read_only("some-volume", "/references/linux").expect("volume"),
        );

        assert!(matches!(
            plan.creation_mounts(&[shadowing]),
            Err(DockerError::InvalidMount(_))
        ));
    }

    #[test]
    fn mcp_sidecar_plan_keeps_read_only_mounts_and_generation() {
        let plan = AgentMcpSidecarPlan::new(
            "agent-1",
            generation(),
            "container-1",
            "ghcr.io/zr233/mai-team-sidecar:latest",
            vec![reference_mount("linux", "/srv/mai-references/linux/repo")],
        )
        .expect("sidecar plan");

        assert_eq!(plan.generation(), generation());
        assert_eq!(plan.mounts().len(), 1);
        assert!(plan.mounts()[0].read_only_bind().is_some());
        assert!(matches!(
            AgentMcpSidecarPlan::new(
                "agent-1",
                generation(),
                " ",
                "ghcr.io/zr233/mai-team-sidecar:latest",
                Vec::new()
            ),
            Err(DockerError::CommandFailed(_))
        ));
    }
}
