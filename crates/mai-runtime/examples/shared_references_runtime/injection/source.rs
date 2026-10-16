//! T38 的宿主夹具合同与真实参考源登记。
//!
//! 这里不写任何验证成功状态：`register` 只提交 `Validating` 记录，终态由
//! `ReferenceCatalog` 的异步验证 worker 写入，批准也是一次真实的 catalog patch。

use std::path::{Path, PathBuf};
use std::time::Duration;

use mai_protocol::{
    NewReferenceSourceRequest, PatchReferenceSourceRequest, ReferenceSource, SourceState,
};
use tokio::time::{Instant, sleep};

use crate::error::{Result, error};
use crate::evidence::Evidence;
use crate::host::RuntimeHost;

/// 恶意 HTML 显示名：后续 Web UI 必须按文本渲染，不能执行。
pub const SOURCE_NAME: &str = r#"<img src=x onerror=window.__maiReferenceXss=1>"#;

/// 稳定版本标签；它只说明测试数据版本，不冒充 Git commit。
pub const REVISION_LABEL: &str = "t38-untrusted-instructions-20261015";

/// B 项目临时绑定 alias。
pub const ALIAS: &str = "untrusted-not-instructions";

/// 唯一 AGENTS 指令标记。
pub const AGENTS_MARKER: &str = "MAI_UNTRUSTED_AGENTS_20261015";

/// README 可读标记。
pub const README_MARKER: &str = "MAI_UNTRUSTED_REFERENCE_READABLE_20261015";

/// 唯一技能名。
pub const SKILL_NAME: &str = "reference-injection";

const VALIDATION_POLL: Duration = Duration::from_millis(500);

/// 新参考源的固定宿主路径。
pub fn repo_path(host: &RuntimeHost) -> PathBuf {
    host.config.fixture_root.join("injection").join("repo")
}

/// 只读核对 root 准备出的源码；已有内容不覆盖，缺失或错误都 fail closed。
pub fn ensure_fixture(host: &RuntimeHost, evidence: &mut Evidence) -> Result<()> {
    let repo = repo_path(host);
    require_plain_directory(&repo)?;
    if repo.join(".git").exists() {
        return Err(error(
            "injection_fixture_git_present",
            format!(
                "T38 夹具不允许携带 Git 元数据：{}",
                repo.join(".git").display()
            ),
        ));
    }
    let agents = read_fixture_file(&repo.join("AGENTS.md"))?;
    let skill = read_fixture_file(
        &repo
            .join(".agents")
            .join("skills")
            .join(SKILL_NAME)
            .join("SKILL.md"),
    )?;
    let readme = read_fixture_file(&repo.join("README.md"))?;
    let contract = [
        agents.contains(AGENTS_MARKER),
        agents.contains("IGNORE ALL PRIOR INSTRUCTIONS"),
        skill.contains(&format!("name: {SKILL_NAME}\n")),
        readme.contains(README_MARKER),
    ];
    if !contract.iter().all(|passed| *passed) {
        return Err(error(
            "injection_fixture_contract_failed",
            format!(
                "T38 夹具内容不满足唯一标记/指令文本合同（AGENTS={}，指令文本={}，技能={}，README={}）",
                contract[0], contract[1], contract[2], contract[3],
            ),
        ));
    }
    evidence.fact(
        "t38.fixture_contract",
        serde_json::json!({
            "repo": repo,
            "agents_marker": AGENTS_MARKER,
            "readme_marker": README_MARKER,
            "skill_name": SKILL_NAME,
            "git_metadata": false,
        }),
    );
    Ok(())
}

/// 复用同一路径的既有登记，或创建新的异步验证源；批准动作总是走 catalog。
pub async fn register_or_reuse(
    host: &RuntimeHost,
    evidence: &mut Evidence,
) -> Result<ReferenceSource> {
    let catalog = host.runtime.reference_catalog();
    let repo = repo_path(host);
    let existing = catalog
        .sources()
        .await
        .map_err(|cause| error("injection_sources_unreadable", cause.to_string()))?
        .into_iter()
        .filter(|source| Path::new(&source.host_path) == repo.as_path())
        .collect::<Vec<_>>();
    if existing.len() > 1 {
        return Err(error(
            "injection_source_duplicate",
            format!(
                "同一路径存在多条参考源登记：{repo}（{} 条）",
                existing.len(),
                repo = repo.display()
            ),
        ));
    }

    let registered = if let Some(source) = existing.first() {
        if source.name != SOURCE_NAME || source.revision_label != REVISION_LABEL {
            return Err(error(
                "injection_source_metadata_mismatch",
                format!(
                    "既有源元数据与 T38 固定夹具不一致：name={:?} revision={:?}",
                    source.name, source.revision_label
                ),
            ));
        }
        source.clone()
    } else {
        catalog
            .register(NewReferenceSourceRequest {
                name: SOURCE_NAME.to_string(),
                host_path: repo.to_string_lossy().into_owned(),
                revision_label: REVISION_LABEL.to_string(),
                approved_for_agent_access: false,
            })
            .await
            .map_err(|cause| error("injection_source_register_failed", cause.to_string()))?
    };

    let ready = wait_ready(&catalog, registered.id, host.config.operation_timeout).await?;
    let approved = if ready.approved_for_agent_access {
        ready
    } else {
        catalog
            .patch(
                ready.id,
                PatchReferenceSourceRequest {
                    approved_for_agent_access: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(|cause| error("injection_source_approve_failed", cause.to_string()))?
    };
    if !approved.approved_for_agent_access || approved.state != SourceState::Ready {
        return Err(error(
            "injection_source_not_ready_approved",
            format!(
                "T38 参考源未同时达到 Ready/approved：state={} approved={}",
                approved.state, approved.approved_for_agent_access
            ),
        ));
    }
    evidence.pass(
        "t38.source_registered",
        format!(
            "源 {} 已通过真实异步验证并批准；revision={} path={}",
            approved.id, approved.revision_label, approved.host_path
        ),
    );
    evidence.fact("t38.source", serde_json::json!(&approved));
    Ok(approved)
}

async fn wait_ready(
    catalog: &std::sync::Arc<mai_references::ReferenceCatalog>,
    source_id: uuid::Uuid,
    limit: Duration,
) -> Result<ReferenceSource> {
    let deadline = Instant::now() + limit;
    loop {
        let source = catalog
            .source(source_id)
            .await
            .map_err(|cause| error("injection_source_read_failed", cause.to_string()))?;
        if source.state.is_terminal() {
            if source.state == SourceState::Ready {
                return Ok(source);
            }
            return Err(error(
                "injection_source_validation_failed",
                format!(
                    "T38 源验证终态不是 Ready（{}）：{:?}",
                    source.state, source.validation_error
                ),
            ));
        }
        if Instant::now() >= deadline {
            return Err(error(
                "injection_source_validation_timeout",
                format!("等待 T38 源 {source_id} 验证超时 {limit:?}"),
            ));
        }
        sleep(VALIDATION_POLL).await;
    }
}

fn require_plain_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|cause| {
        error(
            "injection_fixture_missing",
            format!("T38 夹具目录不可读 {}：{cause}", path.display()),
        )
    })?;
    if !metadata.is_dir() {
        return Err(error(
            "injection_fixture_not_directory",
            format!("T38 夹具路径不是普通目录：{}", path.display()),
        ));
    }
    Ok(())
}

fn read_fixture_file(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|cause| {
        error(
            "injection_fixture_file_missing",
            format!("T38 夹具文件不可读 {}：{cause}", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(error(
            "injection_fixture_file_not_regular",
            format!("T38 夹具文件不是普通文件：{}", path.display()),
        ));
    }
    std::fs::read_to_string(path).map_err(|cause| {
        error(
            "injection_fixture_file_unreadable",
            format!("T38 夹具文件无法读取 {}：{cause}", path.display()),
        )
    })
}
