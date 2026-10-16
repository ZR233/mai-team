//! 登记参考源的事实采集（含廉价的目录身份复核）。

use super::*;

/// 采集一个登记参考源的事实（含廉价身份复核）。
#[derive(Debug, Clone, Serialize)]
pub struct SourceFacts {
    pub role: String,
    pub source_id: Uuid,
    pub host_path: String,
    pub state: String,
    pub approved_for_agent_access: bool,
    pub manifest_digest: Option<String>,
    pub provenance: String,
    pub file_count: u64,
    pub byte_count: u64,
    pub source_device: u64,
    pub source_inode: u64,
    pub validation_error: Option<String>,
    /// 廉价身份复核：当前目录的 dev/ino 是否与登记一致。
    pub identity_matches: Option<bool>,
    pub identity_note: String,
}

/// 采集登记源事实。
pub async fn collect_source_facts(
    probe: &Probe,
    role: &str,
    source_id: Uuid,
) -> Result<SourceFacts> {
    let source = probe.catalog.source(source_id).await?;
    let (identity_matches, identity_note) =
        match mai_references::validate_source_root(&probe.policy, &source.host_path) {
            Ok(root) => {
                let matches =
                    root.device() == source.source_device && root.inode() == source.source_inode;
                (
                    Some(matches),
                    if matches {
                        "目录 dev/ino 与登记一致".to_string()
                    } else {
                        format!(
                            "目录身份变化：登记 {}:{}，当前 {}:{}",
                            source.source_device,
                            source.source_inode,
                            root.device(),
                            root.inode()
                        )
                    },
                )
            }
            Err(error) => (
                None,
                format!("无法复核源根：{}: {}", error.code(), error.message()),
            ),
        };
    Ok(SourceFacts {
        role: role.to_string(),
        source_id: source.id,
        host_path: source.host_path.clone(),
        state: source.state.to_string(),
        approved_for_agent_access: source.approved_for_agent_access,
        manifest_digest: source.manifest_digest.clone(),
        provenance: format!("{:?}", source.provenance.kind).to_lowercase(),
        file_count: source.file_count,
        byte_count: source.byte_count,
        source_device: source.source_device,
        source_inode: source.source_inode,
        validation_error: source
            .validation_error
            .as_ref()
            .map(|failure| format!("{}: {}", failure.code, failure.message)),
        identity_matches,
        identity_note,
    })
}

/// 尚未登记（或已注销）的源：仍要出现在事实里，判定会给出 blocked，而不是静默跳过。
pub fn absent_source_facts(role: &str, path: &std::path::Path) -> SourceFacts {
    SourceFacts {
        role: role.to_string(),
        source_id: Uuid::nil(),
        host_path: path.to_string_lossy().into_owned(),
        state: "unregistered".to_string(),
        approved_for_agent_access: false,
        manifest_digest: None,
        provenance: "unknown".to_string(),
        file_count: 0,
        byte_count: 0,
        source_device: 0,
        source_inode: 0,
        validation_error: None,
        identity_matches: None,
        identity_note: "尚未登记该参考源".to_string(),
    }
}
