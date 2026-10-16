//! 参考源登记与部署策略：写策略文件、只补缺失的夹具源码、登记并等待验证进入终态。

use super::*;

/// 写出部署策略文件（驱动自己的配置，内容确定，重复运行幂等）。
pub(super) fn write_policy_file(config: &ProbeConfig) -> Result<()> {
    let roots = config
        .allowed_roots()
        .iter()
        .map(|path| format!("\"{}\"", path.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!(
        "# 由 shared_references_probe 生成：参考源允许根只来自部署文件，管理 API 不能扩大。\n\
         allowed_roots = [{roots}]\n\
         trusted_owner_uids = [0]\n\
         max_bindings_per_project = 16\n"
    );
    let current = std::fs::read_to_string(&config.policy_path).ok();
    if current.as_deref() == Some(text.as_str()) {
        return Ok(());
    }
    std::fs::write(&config.policy_path, text).map_err(|error| {
        ProbeError::new(
            "policy_write_failed",
            format!("无法写入部署策略 {}：{error}", config.policy_path.display()),
        )
    })?;
    Ok(())
}

/// 夹具源码：只补齐缺失的文件，绝不删除、绝不覆盖已有内容。
pub(super) fn ensure_fixture_files(config: &ProbeConfig, report: &mut Report) -> Result<()> {
    let root = &config.fixture_root;
    let repo = &config.fixture_repo;
    guard::ensure_directory(root)?;
    guard::ensure_directory(repo)?;
    guard::ensure_directory(&repo.join("src"))?;
    let mut created = Vec::new();
    for (path, content) in [
        (
            repo.join("README.md"),
            "# A 项目私有参考夹具\n\n这是验收夹具，只读挂载给 A 项目；B 项目与非项目 Agent 不得访问。\n",
        ),
        (
            repo.join("src").join("fixture.c"),
            "int fixture(void) { return 1; }\n",
        ),
    ] {
        if !path.exists() {
            std::fs::write(&path, content).map_err(|error| {
                ProbeError::new(
                    "fixture_write_failed",
                    format!("无法写入夹具文件 {}：{error}", path.display()),
                )
            })?;
            created.push(path.to_string_lossy().into_owned());
        }
    }
    report.fact(
        "fixture_files",
        json!({ "root": root, "repo": repo, "created": created }),
    )?;
    Ok(())
}

/// 源登记模式：`Ensure` 会登记缺失的源，`ReadOnly` 只查不改（status 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceMode {
    Ensure,
    ReadOnly,
}

/// 登记/复用两个参考源，并等待验证进入终态。
pub(super) async fn resolve_sources(
    probe: &mut Probe,
    mode: SourceMode,
    report: &mut Report,
) -> Result<()> {
    let wanted = vec![
        (
            "linux",
            probe.config.linux_source.clone(),
            format!(
                "Linux 固定版本快照（{}）",
                short_commit(&probe.config.linux_source)
            ),
        ),
        (
            "private-fixture",
            probe.config.fixture_repo.clone(),
            "A 项目私有验收夹具（管理员手工目录）".to_string(),
        ),
    ];
    let existing = probe.catalog.sources().await?;
    let mut resolved = Vec::new();
    for (role, path, name) in wanted {
        let path_text = path.to_string_lossy().into_owned();
        let found = existing
            .iter()
            .find(|source| source.host_path == path_text)
            .cloned();
        let source = match found {
            Some(source) => source,
            None if mode == SourceMode::ReadOnly => {
                report.check(Check::blocked(
                    "sources.registration_missing",
                    format!("{role} 尚未登记（status 只读，不代为登记）：{path_text}"),
                ));
                resolved.push((role, None));
                continue;
            }
            None => {
                let request = NewReferenceSourceRequest {
                    name: name.clone(),
                    host_path: path_text.clone(),
                    revision_label: "验收登记".to_string(),
                    approved_for_agent_access: true,
                };
                probe.catalog.register(request).await?
            }
        };
        let ready = wait_ready(probe, &source, report, role).await?;
        match ready {
            Some(source) => {
                report.fact(
                    format!("source_{role}").as_str(),
                    json!({
                        "source_id": source.id,
                        "host_path": source.host_path,
                        "state": source.state.to_string(),
                        "manifest_digest": source.manifest_digest,
                        "provenance": source.provenance,
                        "file_count": source.file_count,
                        "byte_count": source.byte_count,
                        "source_device": source.source_device,
                        "source_inode": source.source_inode,
                        "validation_error": source.validation_error,
                    }),
                )?;
                resolved.push((role, Some(source.id)));
            }
            None => resolved.push((role, None)),
        }
    }
    for (role, id) in resolved {
        match role {
            "linux" => probe.sources.linux = id,
            _ => probe.sources.private_fixture = id,
        }
    }
    Ok(())
}

/// 等待登记源进入终态；失败时把验证错误写进验收点。
async fn wait_ready(
    probe: &Probe,
    source: &ReferenceSource,
    report: &mut Report,
    role: &str,
) -> Result<Option<ReferenceSource>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        let current = probe.catalog.source(source.id).await?;
        if current.state.is_terminal() {
            if current.state.is_ready() {
                return Ok(Some(current));
            }
            let message = current
                .validation_error
                .as_ref()
                .map(|failure| format!("{}: {}", failure.code, failure.message))
                .unwrap_or_else(|| current.state.to_string());
            report.check(Check::fail(
                format!("sources.{role}.ready"),
                format!("登记源 {role} 未通过验证：{message}"),
            ));
            return Ok(None);
        }
        if tokio::time::Instant::now() > deadline {
            report.check(Check::blocked(
                format!("sources.{role}.ready"),
                "登记源验证在 600 秒内没有进入终态",
            ));
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn short_commit(path: &std::path::Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "未知版本".to_string())
}
