//! 源登记状态、源根身份与内容摘要的判定（含跨轮摘要一致）。

use super::*;

/// 登记源必须 ready 且已确认可供 Agent 访问。
pub(super) fn sources_ready(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("sources.registry_ready");
    if input.sources.is_empty() {
        outcome.block("没有采集到任何登记源");
    }
    for source in input.sources {
        let label = format!("{}：{}", source.role, source.host_path);
        if source.state == "ready" && source.approved_for_agent_access {
            outcome.pass(format!("{label} 已 ready 且已批准"));
        } else {
            outcome.fail(format!(
                "{label} 状态 {} / approved={} / 失败记录 {:?}",
                source.state, source.approved_for_agent_access, source.validation_error
            ));
        }
    }
    outcome.finish("全部登记源 ready 且已批准供 Agent 访问")
}

/// 源根身份（dev/ino + 路径）必须与登记一致——外部替换源根必须被识别。
pub(super) fn source_root_identity(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T33.source_root_identity");
    for source in input.sources {
        match source.identity_matches {
            Some(true) => outcome.pass(format!("{}：{} 身份一致", source.role, source.host_path)),
            Some(false) => outcome.fail(format!(
                "{}：{} 身份变化（{}）",
                source.role, source.host_path, source.identity_note
            )),
            None => outcome.block(format!(
                "{}：无法复核源根（{}）",
                source.role, source.identity_note
            )),
        }
    }
    outcome.finish("源根路径与 dev/ino 与登记一致")
}

/// 深度重验证：需要显式开启（会重新扫描整棵源树）。
pub(super) async fn source_content_digest(probe: &Probe, input: &CheckInput<'_>) -> Result<Check> {
    let mut outcome = Outcome::new("T33.source_content_digest");
    if !probe.config.revalidate_sources {
        outcome.block("未开启 --revalidate-sources：本轮未做深度内容重验证");
        return Ok(outcome.finish("源内容摘要与登记一致"));
    }
    for source in input.sources {
        let record = probe.catalog.source(source.source_id).await?;
        match mai_references::revalidate_source(&probe.policy, &record, chrono::Utc::now()) {
            Ok(validated) => outcome.pass(format!(
                "{}：内容摘要 {} 复核一致",
                source.role,
                validated.identity().manifest_digest()
            )),
            Err(error) => outcome.fail(format!(
                "{}：{}（{}）",
                source.role,
                error.code(),
                error.message()
            )),
        }
    }
    Ok(outcome.finish("源内容摘要与登记一致（身份变化会被识别）"))
}

/// 跨轮比较源内容摘要：整段验收里源必须没被改过。
pub(super) fn host_digest_unchanged(input: &CheckInput<'_>) -> Check {
    let mut outcome = Outcome::new("T22.source_digest_unchanged_across_runs");
    let Some(previous) = input.previous else {
        outcome.block("没有上一轮证据可比");
        return outcome.finish("源内容摘要跨轮一致");
    };
    let previous_sources = sources_by_role(previous);
    for source in input.sources {
        let Some(before) = previous_sources.get(&source.role) else {
            outcome.block(format!("{}：上一轮没有该源的摘要", source.role));
            continue;
        };
        let before_digest = before.get("manifest_digest").and_then(Value::as_str);
        let after_digest = source.manifest_digest.as_deref();
        match (before_digest, after_digest) {
            (Some(before_digest), Some(after_digest)) if before_digest == after_digest => {
                outcome.pass(format!("{}：摘要 {} 跨轮一致", source.role, after_digest))
            }
            (Some(before_digest), Some(after_digest)) => outcome.fail(format!(
                "{}：摘要从 {} 变为 {}（宿主原件被改动）",
                source.role, before_digest, after_digest
            )),
            _ => outcome.block(format!("{}：缺少可比的内容摘要", source.role)),
        }
    }
    outcome.finish("源内容摘要跨轮一致，宿主原件未被改动")
}

/// 上一轮证据里的源事实索引（按 role）。
pub(super) fn sources_by_role(entry: &Value) -> BTreeMap<String, Value> {
    let mut indexed = BTreeMap::new();
    let Some(sources) = entry.pointer("/facts/sources").and_then(Value::as_array) else {
        return indexed;
    };
    for source in sources {
        if let Some(role) = source.get("role").and_then(Value::as_str) {
            indexed.insert(role.to_string(), source.clone());
        }
    }
    indexed
}

/// 按 `--host-digest` 范围计算参考源的宿主摘要。
pub(super) fn source_digest_for(
    probe: &Probe,
    reference: &AppliedReference,
) -> Result<Option<String>> {
    let is_fixture = reference.host_path == probe.config.fixture_repo.to_string_lossy();
    match probe.config.host_digest {
        crate::config::DigestScope::None => Ok(None),
        crate::config::DigestScope::Fixture if !is_fixture => Ok(None),
        crate::config::DigestScope::Fixture | crate::config::DigestScope::All => Ok(Some(
            environment::host_digest(probe, std::path::Path::new(&reference.host_path))?,
        )),
    }
}
