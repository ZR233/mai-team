//! T29：真实 recover/delete/reference-apply 竞争的隔离验收。
//!
//! 不扩张生产 API，直接调用 crate 内部 canonical 恢复入口；所有结果来自真实
//! Docker、产品库和参考应用状态机，证据文件不模拟成功。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mai_docker::DockerClient;
use mai_protocol::{
    AgentId, AgentResourceState, AgentRole, AgentSummary, ApplyProjectReferencesRequest,
    CreateAgentRequest, ProjectId, ProjectReferenceApplicationState, ProjectReferenceBinding,
    ProjectReferenceConfiguration, ReferenceApplyPolicy, ReferenceStatusQuery,
    ReplaceProjectReferencesRequest,
};
use mai_store::MaiStore;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tokio::sync::Barrier;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::{AgentRuntime, RuntimeConfig, RuntimeError, agents};

const ROOT: &str = "/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources";
const DOCKER_HOST: &str = "unix:///run/mai-reference-acceptance-docker/docker.sock";
const AGENT_IMAGE: &str = "sha256:46efd2ec0c7102db5df390e1aef998c7de4666d9db3f7e5dfbfbcae532d30e5b";
const AGENT_IMAGE_ID: &str =
    "sha256:46efd2ec0c7102db5df390e1aef998c7de4666d9db3f7e5dfbfbcae532d30e5b";
const SIDECAR_IMAGE: &str =
    "sha256:19cdf22873afa204799c7fe69dbee33fa3be86512245b1b67a889dc334bd62bb";
const PROJECT_A: &str = "9474b06a-1302-506f-80ca-ec7ace5a7088";
const MAINTAINER: &str = "daa4a5d9-6951-5991-bf14-22f114723fbe";
const PRIVATE_ALIAS: &str = "private-fixture";
const OPERATION: Duration = Duration::from_secs(900);

fn call_evidence<T>(result: &Result<T, RuntimeError>, started: DateTime<Utc>) -> Value {
    let allowed = result.as_ref().err().is_some_and(deletion_won);
    let (result, error) = match result {
        Ok(_) => ("ok", None),
        Err(error) => ("error", Some(error.to_string())),
    };
    json!({"result": result, "allowed_error": allowed, "error": error, "started_at": started, "ended_at": Utc::now()})
}

fn timeout_evidence(started: DateTime<Utc>, message: &str) -> Value {
    call_evidence::<()>(
        &Err(RuntimeError::InvalidInput(message.to_string())),
        started,
    )
}

fn deletion_won(error: &RuntimeError) -> bool {
    matches!(error, RuntimeError::AgentNotFound(_))
        || matches!(error, RuntimeError::InvalidInput(message) if message.contains("is being retired"))
}

fn uuid(text: &str) -> Result<Uuid, RuntimeError> {
    Uuid::parse_str(text).map_err(|error| RuntimeError::InvalidInput(error.to_string()))
}

fn guard(root: &Path) -> Result<String, RuntimeError> {
    if fs::metadata("/proc/self")?.uid() != 0 || fs::canonicalize(root)? != root {
        return Err(RuntimeError::InvalidInput(
            "验收根与 UID0 边界不匹配".to_string(),
        ));
    }
    let docker_host = std::env::var("DOCKER_HOST")
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    if docker_host != DOCKER_HOST {
        return Err(RuntimeError::InvalidInput(format!(
            "DOCKER_HOST 必须为 {DOCKER_HOST}，实际为 {docker_host}"
        )));
    }
    Ok(docker_host)
}

fn config(root: &Path) -> RuntimeConfig {
    RuntimeConfig {
        references_policy_path: Some(root.join("config").join("reference-sources.toml")),
        reference_application_observer: None,
        repo_root: root.to_path_buf(),
        projects_root: root.join("projects"),
        sessions_root: root.join("data").join("sessions"),
        cache_root: root.join("cache"),
        artifact_files_root: root.join("artifacts"),
        sidecar_image: SIDECAR_IMAGE.to_string(),
        github_api_base_url: None,
        git_binary: None,
        system_skills_root: Some(root.join("runtime-resources").join("system-skills")),
        system_agents_root: Some(root.join("runtime-resources").join("system-agents")),
    }
}

fn private(
    configuration: &ProjectReferenceConfiguration,
) -> Result<ProjectReferenceBinding, RuntimeError> {
    configuration
        .bindings
        .iter()
        .find(|item| item.alias == PRIVATE_ALIAS)
        .cloned()
        .ok_or_else(|| RuntimeError::InvalidInput("缺少 private-fixture 绑定".to_string()))
}

async fn wait_applied(runtime: &AgentRuntime, project: ProjectId) -> Result<(), RuntimeError> {
    let deadline = tokio::time::Instant::now() + OPERATION;
    loop {
        let status = runtime
            .project_reference_status(project, ReferenceStatusQuery::default())
            .await?;
        match status.state {
            ProjectReferenceApplicationState::Applied => return Ok(()),
            ProjectReferenceApplicationState::Failed => {
                return Err(RuntimeError::InvalidInput(format!(
                    "参考应用失败：{:?}",
                    status.counts
                )));
            }
            ProjectReferenceApplicationState::Pending
            | ProjectReferenceApplicationState::Deferred => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RuntimeError::InvalidInput(format!(
                "等待 Applied 超时：{:?}",
                status.counts
            )));
        }
        sleep(Duration::from_secs(1)).await;
    }
}

async fn create_executor(
    runtime: &Arc<AgentRuntime>,
    project: ProjectId,
    parent_id: AgentId,
) -> Result<AgentSummary, RuntimeError> {
    let parent = runtime
        .list_agents()
        .await
        .into_iter()
        .find(|item| item.id == parent_id)
        .ok_or(RuntimeError::AgentNotFound(parent_id))?;
    runtime
        .create_agent_with_container_source(
            CreateAgentRequest {
                name: Some("t29-recovery-race-executor".to_string()),
                provider_id: Some(parent.provider_id.clone()),
                model: Some(parent.model.clone()),
                reasoning_effort: parent.reasoning_effort.clone(),
                docker_image: Some(AGENT_IMAGE.to_string()),
                parent_id: Some(parent.id),
                system_prompt: None,
            },
            agents::ContainerSource::FreshImage,
            None,
            Some(project),
            Some(AgentRole::Executor),
        )
        .await
}

async fn prepare_and_race(
    runtime: &Arc<AgentRuntime>,
    store: &Arc<MaiStore>,
    project: ProjectId,
    maintainer: AgentId,
    evidence: &mut Value,
) -> Result<(), String> {
    wait_applied(runtime, project)
        .await
        .map_err(|error| error.to_string())?;
    let before = runtime
        .list_agents()
        .await
        .into_iter()
        .map(|agent| agent.id)
        .collect::<BTreeSet<_>>();
    evidence["before_agent_ids"] = json!(before);
    let configuration = runtime
        .project_reference_configuration(project)
        .await
        .map_err(|error| error.to_string())?;
    let original = private(&configuration).map_err(|error| error.to_string())?;
    evidence["original_private_binding"] = json!(original);
    let temporary = create_executor(runtime, project, maintainer)
        .await
        .map_err(|error| error.to_string())?;
    evidence["temporary_agent_id"] = json!(temporary.id);

    let binding = store
        .reference_binding(temporary.id)
        .await
        .map_err(|error| error.to_string())?;
    let container_id = temporary.container_id.clone().unwrap_or_default();
    let facts = runtime
        .deps
        .docker
        .inspect_container_environment(&container_id)
        .await
        .map_err(|error| error.to_string())?;
    let binding_ready = binding.is_some_and(|binding| {
        binding.project_id == Some(project)
            && binding.container_id == container_id
            && binding.applied_revision == configuration.revision
    });
    evidence["initial"] = json!({
        "shared_start_revision": configuration.revision,
        "agent_image_matches": facts.image_id == AGENT_IMAGE_ID,
        "agent_container_running": facts.running,
        "binding_ready": binding_ready,
        "sidecar_digest_configured": true,
    });
    if temporary.resource.state != AgentResourceState::Ready
        || container_id.is_empty()
        || facts.image_id != AGENT_IMAGE_ID
        || !facts.running
        || !binding_ready
    {
        return Err("临时 Executor 初始容器或 binding 未 Ready".to_string());
    }

    let id = temporary.id;
    let mut bindings = configuration.bindings.clone();
    bindings
        .iter_mut()
        .find(|item| item.id == original.id)
        .ok_or_else(|| "缺少原绑定 ID".to_string())?
        .alias = format!("{PRIVATE_ALIAS}-t29-race");
    let barrier = Arc::new(Barrier::new(3));
    let recovery_runtime = Arc::clone(runtime);
    let recovery_barrier = Arc::clone(&barrier);
    let recovery_started = Utc::now();
    let recovery_fut = async move {
        recovery_barrier.wait().await;
        recovery_runtime
            .recover_project_agent_resources(
                agents::AgentResourceRecoveryRequest::borrowed_workspace(id),
            )
            .await
    };
    let delete_runtime = Arc::clone(runtime);
    let delete_barrier = Arc::clone(&barrier);
    let delete_started = Utc::now();
    let delete_fut = async move {
        delete_barrier.wait().await;
        delete_runtime.delete_agent(id).await
    };
    let reference_runtime = Arc::clone(runtime);
    let reference_barrier = Arc::clone(&barrier);
    let replace_started = Utc::now();
    let reference_fut = async move {
        reference_barrier.wait().await;
        let replace = reference_runtime
            .replace_project_references(
                project,
                ReplaceProjectReferencesRequest {
                    expected_revision: configuration.revision,
                    bindings,
                },
            )
            .await;
        let replace_evidence = call_evidence(&replace, replace_started);
        let revision = replace.as_ref().ok().map(|saved| saved.revision);
        let apply_started = Utc::now();
        let apply = match revision {
            Some(revision) => reference_runtime
                .apply_project_references(
                    project,
                    ApplyProjectReferencesRequest {
                        expected_revision: revision,
                        policy: ReferenceApplyPolicy::WhenIdle,
                    },
                )
                .await
                .map(|status| status.target_revision),
            None => Err(RuntimeError::InvalidInput(
                "reference CAS 未产生修订".to_string(),
            )),
        };
        let allowed = apply.as_ref().err().is_some_and(deletion_won);
        let apply_evidence = call_evidence(&apply, apply_started);
        let revision = apply.ok();
        let succeeded = revision.is_some();
        (
            json!({"replace": replace_evidence, "apply": apply_evidence, "resulting_revision": revision}),
            replace.is_ok(),
            succeeded || allowed,
            succeeded || allowed,
        )
    };

    let (recovery, deletion, reference) = tokio::join!(
        timeout(OPERATION, recovery_fut),
        timeout(OPERATION, delete_fut),
        timeout(OPERATION, reference_fut),
    );
    let recovery =
        recovery.unwrap_or_else(|_| Err(RuntimeError::InvalidInput("recovery 超时".to_string())));
    let deletion =
        deletion.unwrap_or_else(|_| Err(RuntimeError::InvalidInput("delete 超时".to_string())));
    evidence["recovery"] = call_evidence(&recovery, recovery_started);
    evidence["deletion"] = call_evidence(&deletion, delete_started);
    let (reference_evidence, replace_ok, apply_ok, revision_ok) = match reference {
        Ok(item) => item,
        Err(_) => (
            json!({
                "replace": timeout_evidence(replace_started, "reference CAS 超时"),
                "apply": timeout_evidence(replace_started, "reference apply 超时"),
                "resulting_revision": null,
            }),
            false,
            false,
            false,
        ),
    };
    evidence["reference_change"] = reference_evidence;
    if let Err(error) = deletion
        && !deletion_won(&error)
    {
        return Err(format!("删除竞态未允许错误：{error}"));
    }
    if let Err(error) = recovery
        && !deletion_won(&error)
    {
        return Err(format!("恢复竞态未允许错误：{error}"));
    }
    if !replace_ok || !apply_ok || !revision_ok {
        return Err("reference CAS 或 apply 未允许失败".to_string());
    }
    Ok(())
}

async fn restore(
    runtime: &Arc<AgentRuntime>,
    project: ProjectId,
    original: &ProjectReferenceBinding,
) -> Result<(Value, u64), RuntimeError> {
    let current = runtime.project_reference_configuration(project).await?;
    let already = current
        .bindings
        .iter()
        .any(|item| item.id == original.id && item.alias == original.alias);
    let started = Utc::now();
    let saved = if already {
        current
    } else {
        let mut bindings = current.bindings.clone();
        let position = bindings
            .iter()
            .position(|item| item.id == original.id)
            .ok_or_else(|| RuntimeError::InvalidInput("还原时缺少原绑定 ID".to_string()))?;
        bindings[position].alias = original.alias.clone();
        runtime
            .replace_project_references(
                project,
                ReplaceProjectReferencesRequest {
                    expected_revision: current.revision,
                    bindings,
                },
            )
            .await?
    };
    let replace_evidence = if already {
        json!({"result": "noop_current_already_original", "allowed_error": false, "error": null,
            "started_at": started, "ended_at": Utc::now()})
    } else {
        call_evidence(&Ok(()), started)
    };
    let apply_started = Utc::now();
    let apply = runtime
        .apply_project_references(
            project,
            ApplyProjectReferencesRequest {
                expected_revision: saved.revision,
                policy: ReferenceApplyPolicy::WhenIdle,
            },
        )
        .await;
    let apply_evidence = call_evidence(&apply, apply_started);
    apply?;
    let wait_started = Utc::now();
    let wait = wait_applied(runtime, project).await;
    let wait_evidence = call_evidence(&wait, wait_started);
    wait?;
    let revision = runtime
        .project_reference_configuration(project)
        .await?
        .revision;
    Ok((
        json!({"replace": replace_evidence, "apply": apply_evidence, "wait": wait_evidence, "final_revision": revision}),
        revision,
    ))
}

async fn final_checks(
    runtime: &Arc<AgentRuntime>,
    store: &Arc<MaiStore>,
    project: ProjectId,
    temporary: Option<AgentId>,
    before: &BTreeSet<AgentId>,
    original_id: Uuid,
) -> Result<(Value, Option<ProjectReferenceBinding>), RuntimeError> {
    let configuration = runtime.project_reference_configuration(project).await?;
    let status = runtime
        .project_reference_status(project, ReferenceStatusQuery::default())
        .await?;
    let runtime_ids = runtime
        .list_agents()
        .await
        .into_iter()
        .map(|agent| agent.id)
        .collect::<BTreeSet<_>>();
    let persisted = store
        .load_runtime_snapshot(0)
        .await?
        .agents
        .into_iter()
        .map(|agent| agent.summary.id)
        .collect::<BTreeSet<_>>();
    let temporary_binding = match temporary {
        Some(id) => store.reference_binding(id).await?,
        None => None,
    };
    let temporary_text = temporary.map(|id| id.to_string());
    let managed = runtime.deps.docker.list_managed_containers().await?;
    let containers = managed
        .iter()
        .filter(|item| item.agent_id.as_deref() == temporary_text.as_deref())
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    let mut owners = BTreeMap::<String, BTreeSet<String>>::new();
    for item in managed.iter().filter(|item| !item.sidecar) {
        if let Some(agent) = &item.agent_id {
            owners
                .entry(agent.clone())
                .or_default()
                .insert(item.id.clone());
        }
    }
    let state = match status.state {
        ProjectReferenceApplicationState::Applied => "applied",
        ProjectReferenceApplicationState::Pending => "pending",
        ProjectReferenceApplicationState::Failed => "failed",
        ProjectReferenceApplicationState::Deferred => "deferred",
    };
    let final_binding = configuration
        .bindings
        .iter()
        .find(|item| item.id == original_id)
        .cloned();
    let checks = json!({
        "temporary_agent_runtime_absent": temporary.is_none_or(|id| !runtime_ids.contains(&id)),
        "temporary_agent_store_absent": temporary.is_none_or(|id| !persisted.contains(&id)),
        "temporary_reference_binding_absent": temporary_binding.is_none(),
        "temporary_managed_containers": containers,
        "surviving_original_agents": before.intersection(&persisted).count(),
        "project_agent_total": status.agents.len(),
        "project_agents_with_container": status.agents.iter().filter(|agent| agent.container_id.is_some()).count(),
        "duplicate_physical_containers": owners.into_iter().filter(|(_, owners)| owners.len() > 1).map(|(id, _)| id).collect::<Vec<_>>(),
        "project_reference_state": state,
        "final_revision": configuration.revision,
    });
    Ok((checks, final_binding))
}

#[tokio::test]
#[ignore = "需要目标机隔离 Docker 与固定夹具"]
async fn recover_delete_and_reference_apply_race_has_no_late_resurrection() {
    let started = Utc::now();
    let root = Path::new(ROOT);
    let docker_host = guard(root).expect("T29 隔离边界");
    let project = ProjectId::from(uuid(PROJECT_A).expect("项目 UUID"));
    let maintainer = AgentId::from(uuid(MAINTAINER).expect("Maintainer UUID"));
    let store = Arc::new(
        MaiStore::open_in_data_dir(root.join("data"))
            .await
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
            .expect("打开固定产品库"),
    );
    let runtime = AgentRuntime::new(
        DockerClient::new(AGENT_IMAGE.to_string()),
        Arc::clone(&store),
        config(root),
    )
    .await
    .expect("装配真实 Runtime");
    let mut evidence = json!({
        "task": "T29 recover/delete/reference-apply race", "root": root, "docker_host": docker_host,
        "running_as_uid0": true, "started_at": started, "ended_at": started, "project_id": project,
        "maintainer_id": maintainer, "before_agent_ids": [], "temporary_agent_id": null,
        "original_private_binding": {"id": Uuid::nil(), "source_id": Uuid::nil(), "alias": PRIVATE_ALIAS,
            "description": "", "enabled": true},
        "initial": null, "recovery": null, "deletion": null, "reference_change": null, "cleanup": null,
        "restoration": null, "final_checks": null, "final_private_binding": null, "scenario_error": null,
    });

    if let Err(error) = prepare_and_race(&runtime, &store, project, maintainer, &mut evidence).await
    {
        evidence["scenario_error"] = json!(error);
    }
    if let Some(temporary) = evidence["temporary_agent_id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())
    {
        let started = Utc::now();
        let result = timeout(OPERATION, runtime.delete_agent(AgentId::from(temporary)))
            .await
            .unwrap_or_else(|_| {
                Err(RuntimeError::InvalidInput(
                    "cleanup delete 超时".to_string(),
                ))
            });
        evidence["cleanup"] = call_evidence(&result, started);
    }
    let original = serde_json::from_value::<ProjectReferenceBinding>(
        evidence["original_private_binding"].clone(),
    )
    .expect("原绑定 JSON");
    match restore(&runtime, project, &original).await {
        Ok((value, revision)) => {
            evidence["restoration"] = value;
            evidence["final_revision_hint"] = json!(revision);
        }
        Err(error) => {
            evidence["scenario_error"] = json!(format!("{error}；参考配置还原/应用失败"));
        }
    }
    let before = evidence["before_agent_ids"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().and_then(|text| Uuid::parse_str(text).ok()))
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let temporary = evidence["temporary_agent_id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok());
    // 删除/恢复 future 都已返回；留一个短暂静默窗口再读真实终态，捕捉迟到后台收据。
    sleep(Duration::from_secs(2)).await;
    match final_checks(&runtime, &store, project, temporary, &before, original.id).await {
        Ok((checks, binding)) => {
            evidence["final_checks"] = checks;
            evidence["final_private_binding"] = json!(binding);
        }
        Err(error) => {
            evidence["scenario_error"] = json!(format!("{error}；最终实物检查失败"));
        }
    }
    evidence["ended_at"] = json!(Utc::now());
    let path = root.join("evidence").join("t29-recovery-race.json");
    fs::create_dir_all(path.parent().expect("证据父目录"))
        .and_then(|_| {
            fs::write(
                &path,
                serde_json::to_vec_pretty(&evidence).expect("序列化 T29 证据"),
            )
        })
        .expect("写入 T29 证据");

    let revision = evidence["final_revision_hint"].as_u64().unwrap_or_default();
    let total = evidence["final_checks"]["project_agent_total"]
        .as_u64()
        .unwrap_or_default();
    let expected = json!({
        "temporary_agent_runtime_absent": true, "temporary_agent_store_absent": true,
        "temporary_reference_binding_absent": true, "temporary_managed_containers": [],
        "surviving_original_agents": evidence["before_agent_ids"].as_array().map(Vec::len).unwrap_or_default(),
        "project_agent_total": total, "project_agents_with_container": total,
        "duplicate_physical_containers": [], "project_reference_state": "applied", "final_revision": revision,
    });
    assert_eq!(
        evidence["scenario_error"],
        Value::Null,
        "T29 场景不能存在未分类失败"
    );
    assert_eq!(
        evidence["final_checks"], expected,
        "T29 最终实物检查必须完整收敛"
    );
    assert_eq!(
        evidence["final_private_binding"], evidence["original_private_binding"],
        "必须完整还原 private-fixture 绑定"
    );
}
