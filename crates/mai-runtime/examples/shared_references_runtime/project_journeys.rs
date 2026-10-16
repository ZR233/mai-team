//! `projects` 场景：真实 Runtime 的项目旅程验收（T18 创建项目自带引用、T20 自动 PR 审查）。
//!
//! 本模块只覆盖**项目**入口，与 `tools`/`gate`/`lifecycle`/`channels` 的既有驱动互不干扰：
//! 它自己装配一个带夹具 GitHub App backend 的 `AgentRuntime`，因此：
//! - 项目创建、克隆、首次容器装配、自动审查调度全部走**产品公开入口**；
//! - GitHub 安装/仓库/token 由本地夹具 backend 提供（全假 token，不出网）；
//! - Git 传输仍由产品把仓库 URL 硬编码到 github.com，靠 sidecar 镜像的
//!   `url.http://127.0.0.1:18082/.insteadOf https://github.com/mai-reference-fixture/`
//!   重写到本地夹具（见 `scripts/references/project-fixture.py` 与最终报告里的 root 准备）。
//!
//! 判定只使用真实事实：真实工具 trace（`Runtime::thread_turns`）、真实 Docker 挂载事实、
//! 夹具清单里的 commit SHA 与文件内容。任何未真正执行的断言都记为 `blocked`/`boundary`。
//!
//! 用法（root 在验收机上执行）：
//!
//! ```text
//! DOCKER_HOST=unix:///run/mai-reference-acceptance-docker/docker.sock \
//!   cargo run -p mai-runtime --example shared_references_runtime -- projects
//! ```

#[path = "project_journeys/backend.rs"]
mod backend;
#[path = "project_journeys/cleanup.rs"]
mod cleanup;
#[path = "project_journeys/fixture.rs"]
mod fixture;
#[path = "project_journeys/race.rs"]
mod race;
#[path = "project_journeys/reviewer.rs"]
mod reviewer;
#[path = "project_journeys/support.rs"]
mod support;
#[path = "project_journeys/t18.rs"]
mod t18;
#[path = "project_journeys/t20.rs"]
mod t20;
#[path = "project_journeys/t27.rs"]
mod t27;

use std::sync::Arc;

use mai_docker::DockerClient;
use mai_runtime::{AgentRuntime, RuntimeConfig, github::GithubAppBackend};
use mai_store::MaiStore;
use serde_json::json;

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::host::{self, RuntimeHost};
use crate::runtime_config::DriverConfig;

/// `projects` 命令入口。
pub async fn run(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    let Some(fixture) = fixture::GitHubFixture::load(config, evidence)? else {
        // 夹具缺失时 `load` 已记 blocked，这里不再伪造任何结论。
        return Ok(());
    };
    let api_base_url = fixture::resolve_api_base_url(&fixture);
    evidence.fact("projects.github_api_base_url", json!(&api_base_url));
    let github_backend = Arc::new(backend::FixtureGithubAppBackend::new(&fixture));
    let store = host::open_store(config).await?;
    let host = start(
        config,
        store,
        &api_base_url,
        Arc::clone(&github_backend) as Arc<dyn GithubAppBackend>,
    )
    .await?;
    // 与 `config`/`tools` 场景一致：写 fixture provider 与四个角色路由，
    // 新项目的 maintainer/reviewer 才会冻结到确定性 Responses provider（127.0.0.1:18081）。
    evidence.fact("provider", host.configure_models().await?);
    let outcome = async {
        if let Some(created) = t18::scenario(&host, &fixture, evidence).await? {
            let reviewed: Result<()> = async {
                t27::scenario(&host, &created, evidence).await?;
                race::scenario(&host, &created, evidence).await?;
                t20::scenario(&host, &fixture, &created, evidence).await
            }
            .await;
            let cleaned = cleanup::scenario(&host, &created, evidence).await;
            reviewed?;
            cleaned?;
        }
        Ok(())
    }
    .await;
    evidence.fact(
        "projects.github_backend_calls",
        json!(github_backend.calls()),
    );
    host.shutdown().await;
    outcome
}

/// 装配带夹具 GitHub App backend 的真实 `AgentRuntime`。
///
/// 与 `host::start` 的唯一差别：注入夹具 backend 并把 `github_api_base_url` 指向本地夹具，
/// 因此项目创建与自动审查的 GitHub 读写都落在 `127.0.0.1:18082`，绝不触达真实 GitHub。
pub async fn start(
    config: &DriverConfig,
    store: Arc<MaiStore>,
    github_api_base_url: &str,
    github_backend: Arc<dyn GithubAppBackend>,
) -> Result<RuntimeHost> {
    for (role, dir) in [
        ("项目目录", &config.projects_root),
        ("缓存目录", &config.cache_root),
        ("产物目录", &config.artifacts_root),
        ("会话目录", &config.sessions_root),
    ] {
        std::fs::create_dir_all(dir).map_err(|cause| {
            error(
                "runtime_dir_unwritable",
                format!("无法创建{role} {}：{cause}", dir.display()),
            )
        })?;
    }
    let docker = DockerClient::new(config.agent_image.clone());
    let runtime = AgentRuntime::new_with_github_backend(
        docker.clone(),
        Arc::clone(&store),
        RuntimeConfig {
            references_policy_path: Some(config.policy_path.clone()),
            reference_application_observer: None,
            repo_root: config.root.clone(),
            projects_root: config.projects_root.clone(),
            sessions_root: config.sessions_root.clone(),
            cache_root: config.cache_root.clone(),
            artifact_files_root: config.artifacts_root.clone(),
            sidecar_image: config.sidecar_image.clone(),
            github_api_base_url: Some(github_api_base_url.to_string()),
            git_binary: None,
            system_skills_root: Some(config.system_skills_root.clone()),
            system_agents_root: Some(config.system_agents_root.clone()),
        },
        Some(github_backend),
    )
    .await
    .map_err(|cause| {
        error(
            "runtime_bootstrap_failed",
            format!("项目旅程的 AgentRuntime 装配失败（部署策略/镜像/库状态）：{cause}"),
        )
    })?;
    Ok(RuntimeHost {
        config: config.clone(),
        docker,
        store,
        runtime,
    })
}
