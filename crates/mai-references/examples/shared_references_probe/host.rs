//! 唯一的 `ReferenceEnvironmentHost` 实现：真实停止旧环境、真实创建并核验 sidecar。
//!
//! 三条回调的语义（都能被 root 用 Docker 事实复核）：
//! - `quiesce`：停止该 Agent 现有的一切受管容器（旧执行环境与 MCP sidecar），只 stop
//!   不 delete——候选容器的删除只由状态机按精确 ID 执行；
//! - `rebind`：用 `AgentMcpSidecarPlan` 真实创建 sidecar，再逐项核对参考路径确实
//!   只读递归只读；旧 generation 的 sidecar 必须已经退出，否则本次换代失败；
//! - `activate`：**不启动模型 turn、不恢复 PL Thread**，只记录绑定已发布。真实 Runtime
//!   的 Thread 恢复与工具通道由 root 另测（见报告 boundaries）。

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use mai_docker::{AgentMcpSidecarPlan, ContainerHandle, ContainerMountType, DockerClient};
use mai_protocol::{AgentId, ReferenceApplyPolicy, ReferenceErrorCode};
use mai_references::{ReferenceEnvironmentHost, ReferenceError, reference_mounts};
use mai_store::PersistedReferenceBinding;
use serde::Serialize;
use tokio::sync::Mutex;

/// 一次回调的观察记录。
#[derive(Debug, Clone, Serialize)]
pub struct HostEvent {
    pub callback: &'static str,
    pub agent_id: String,
    pub at: chrono::DateTime<Utc>,
    pub detail: String,
}

/// 回调过程中积累的证据。
#[derive(Debug, Default)]
pub struct HostState {
    pub events: Vec<HostEvent>,
    /// Agent -> 换代前存在的 sidecar ID 列表。
    pub sidecars_before: BTreeMap<String, Vec<String>>,
    /// Agent -> 本次 rebind 建立/复用的 sidecar ID。
    pub sidecar_after: BTreeMap<String, String>,
    /// 旧 generation 的 sidecar ID（必须在换代后消失）。
    pub stale_sidecars: Vec<String>,
    pub rebinds: u64,
    pub activates: u64,
}

/// 验收用的环境回调实现。
pub struct ProbeHost {
    docker: DockerClient,
    sidecar_image: String,
    fault_control: std::path::PathBuf,
    state: Mutex<HostState>,
}

impl ProbeHost {
    /// 用真实 Docker 客户端与 sidecar 镜像构造。
    pub fn new(
        docker: DockerClient,
        sidecar_image: impl Into<String>,
        fault_control: std::path::PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            docker,
            sidecar_image: sidecar_image.into(),
            fault_control,
            state: Mutex::new(HostState::default()),
        })
    }

    /// 取出回调证据（复制一份，供报告序列化）。
    pub async fn evidence(&self) -> HostEvidence {
        let state = self.state.lock().await;
        HostEvidence {
            events: state.events.clone(),
            sidecars_before: state.sidecars_before.clone(),
            sidecar_after: state.sidecar_after.clone(),
            stale_sidecars: state.stale_sidecars.clone(),
            rebinds: state.rebinds,
            activates: state.activates,
        }
    }

    async fn record(&self, callback: &'static str, agent_id: &str, detail: String) {
        let mut state = self.state.lock().await;
        state.events.push(HostEvent {
            callback,
            agent_id: agent_id.to_string(),
            at: Utc::now(),
            detail,
        });
    }

    /// 该 Agent 当前存在的 sidecar 容器 ID。
    async fn sidecars_of(&self, agent_id: &str) -> mai_references::Result<Vec<String>> {
        let containers = self
            .docker
            .list_managed_containers()
            .await
            .map_err(map_docker)?;
        Ok(containers
            .into_iter()
            .filter(|container| {
                container.sidecar && container.agent_id.as_deref() == Some(agent_id)
            })
            .map(|container| container.id)
            .collect())
    }
}

impl ReferenceEnvironmentHost for ProbeHost {
    async fn quiesce(
        &self,
        agent_id: AgentId,
        policy: ReferenceApplyPolicy,
    ) -> mai_references::Result<()> {
        let agent = agent_id.to_string();
        let containers = self
            .docker
            .list_managed_containers()
            .await
            .map_err(map_docker)?;
        let mut stopped_agents = Vec::new();
        let mut stopped_sidecars = Vec::new();
        for container in containers
            .iter()
            .filter(|container| container.agent_id.as_deref() == Some(agent.as_str()))
        {
            self.docker
                .stop_container(&container.id)
                .await
                .map_err(map_docker)?;
            if container.sidecar {
                stopped_sidecars.push(container.id.clone());
            } else {
                stopped_agents.push(container.id.clone());
            }
        }
        // 只供固定隔离驱动注入：容器先停止，随后模拟宿主资源关闭超过产品的 60 秒上限。
        if let Ok(bytes) = std::fs::read(&self.fault_control) {
            let control: serde_json::Value = serde_json::from_slice(&bytes).map_err(|cause| {
                mai_references::ReferenceError::new(
                    mai_protocol::ReferenceErrorCode::InternalError,
                    "故障夹具格式错误",
                )
                .with_cause(cause)
            })?;
            if control["mode"] == "close_timeout" && control["agent_id"] == agent {
                tokio::time::sleep(std::time::Duration::from_secs(65)).await;
            }
        }
        self.record(
            "quiesce",
            &agent,
            format!(
                "policy={policy}；已停止旧执行环境 {stopped_agents:?} 与 sidecar {stopped_sidecars:?}（只 stop，不 delete）"
            ),
        )
        .await;
        Ok(())
    }

    async fn rebind(
        &self,
        binding: PersistedReferenceBinding,
        container: ContainerHandle,
    ) -> mai_references::Result<()> {
        let agent = binding.agent_id.to_string();
        let before = self.sidecars_of(&agent).await?;
        let mounts = reference_mounts(&binding.snapshot)?;
        let plan = AgentMcpSidecarPlan::new(
            agent.clone(),
            binding.generation,
            container.id.clone(),
            self.sidecar_image.clone(),
            mounts,
        )
        .map_err(map_docker)?;
        let sidecar = self
            .docker
            .ensure_agent_mcp_sidecar_environment(&plan)
            .await
            .map_err(map_docker)?;
        let facts = self
            .docker
            .inspect_container_environment(&sidecar.id)
            .await
            .map_err(map_docker)?;
        // 逐项核对：参考路径必须真的只读、递归只读、且指向同一个宿主目录。
        for planned in plan.mounts() {
            let actual = facts.mount_at(planned.target()).ok_or_else(|| {
                ReferenceError::new(
                    ReferenceErrorCode::EnvironmentApplyFailed,
                    format!("sidecar 缺少参考挂载 {}", planned.target()),
                )
            })?;
            let expected_source = planned
                .read_only_bind()
                .map(|mount| mount.source().to_string())
                .unwrap_or_default();
            if actual.mount_type != ContainerMountType::Bind
                || !actual.read_only
                || actual.recursive_read_only != Some(true)
                || actual.source != expected_source
            {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::EnvironmentApplyFailed,
                    format!(
                        "sidecar 参考挂载未通过只读核验：{} -> {}（read_only={}, recursive={:?}）",
                        planned.target(),
                        actual.source,
                        actual.read_only,
                        actual.recursive_read_only
                    ),
                ));
            }
        }
        let stale = before
            .iter()
            .filter(|id| **id != sidecar.id)
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        let remaining = self.sidecars_of(&agent).await?;
        if let Some(left) = stale
            .iter()
            .find(|id| remaining.iter().any(|existing| existing == *id))
        {
            return Err(ReferenceError::new(
                ReferenceErrorCode::EnvironmentApplyFailed,
                format!("旧 generation 的 sidecar {left} 仍然存在，拒绝发布新环境"),
            ));
        }
        {
            let mut state = self.state.lock().await;
            state.sidecars_before.insert(agent.clone(), before);
            state
                .sidecar_after
                .insert(agent.clone(), sidecar.id.clone());
            state.stale_sidecars.extend(stale.clone());
            state.rebinds += 1;
        }
        self.record(
            "rebind",
            &agent,
            format!(
                "generation={} 候选容器={} sidecar={}（旧 sidecar {:?} 已退出）",
                binding.generation, container.id, sidecar.id, stale
            ),
        )
        .await;
        Ok(())
    }

    async fn activate(&self, binding: PersistedReferenceBinding) -> mai_references::Result<()> {
        let agent = binding.agent_id.to_string();
        if let Ok(bytes) = std::fs::read(&self.fault_control) {
            let control: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            if control["mode"] == "crash_activation" && control["agent_id"] == agent {
                let marker = self.fault_control.with_extension("activation.json");
                std::fs::write(&marker, serde_json::to_vec(&binding).expect("绑定序列化"))
                    .expect("崩溃证据");
                std::fs::File::open(marker)
                    .expect("证据文件")
                    .sync_all()
                    .expect("同步证据");
                std::process::exit(137);
            }
        }
        {
            let mut state = self.state.lock().await;
            state.activates += 1;
        }
        self.record(
            "activate",
            &agent,
            format!(
                "已发布绑定 generation={}（本驱动不启动模型 turn/PL Thread）",
                binding.generation
            ),
        )
        .await;
        Ok(())
    }
}

/// 回调证据快照。
#[derive(Debug, Clone, Serialize)]
pub struct HostEvidence {
    pub events: Vec<HostEvent>,
    pub sidecars_before: BTreeMap<String, Vec<String>>,
    pub sidecar_after: BTreeMap<String, String>,
    pub stale_sidecars: Vec<String>,
    pub rebinds: u64,
    pub activates: u64,
}

fn map_docker(error: mai_docker::DockerError) -> ReferenceError {
    ReferenceError::new(
        ReferenceErrorCode::EnvironmentApplyFailed,
        format!("Docker 调用失败：{error}"),
    )
}
