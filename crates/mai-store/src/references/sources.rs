use super::*;

const SOURCE_COLUMNS: &str = "id,name,revision_label,provenance_json,canonical_path,manifest_digest,source_dev,source_ino,state,error,file_count,byte_count,approved_for_agent_access,warnings_json,created_at,updated_at";

fn decode_source(row: &Row<'_>) -> Result<ReferenceSource> {
    let parse_number = |index| -> Result<u64> {
        row.get::<_, String>(index)?
            .parse()
            .map_err(|error| StoreError::DataIntegrity(format!("参考源数值无效: {error}")))
    };
    let digest: String = row.get(5)?;
    let failure: Option<String> = row.get(9)?;
    Ok(ReferenceSource {
        id: Uuid::parse_str(&row.get::<_, String>(0)?)
            .map_err(|error| StoreError::DataIntegrity(error.to_string()))?,
        name: row.get(1)?,
        revision_label: row.get(2)?,
        provenance: serde_json::from_str(&row.get::<_, String>(3)?)?,
        host_path: row.get(4)?,
        manifest_digest: if digest.is_empty() {
            None
        } else {
            Some(digest)
        },
        source_device: parse_number(6)?,
        source_inode: parse_number(7)?,
        state: SourceState::from_str(&row.get::<_, String>(8)?)?,
        validation_error: failure.as_deref().map(serde_json::from_str).transpose()?,
        file_count: parse_number(10)?,
        byte_count: parse_number(11)?,
        approved_for_agent_access: row.get(12)?,
        warnings: serde_json::from_str(&row.get::<_, String>(13)?)?,
        created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(14)?)?.with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(15)?)?.with_timezone(&Utc),
    })
}

pub(super) fn sources_on(connection: &Connection) -> Result<Vec<ReferenceSource>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {SOURCE_COLUMNS} FROM reference_sources ORDER BY created_at,id"
    ))?;
    let mut rows = statement.query([])?;
    let mut sources = Vec::new();
    while let Some(row) = rows.next()? {
        sources.push(decode_source(row)?);
    }
    Ok(sources)
}

impl MaiStore {
    pub async fn reference_sources(&self) -> Result<Vec<ReferenceSource>> {
        let path = self.path.clone();
        sqlite_task(move || sources_on(&connection(&path)?)).await
    }

    pub async fn insert_reference_source(&self, source: ReferenceSource) -> Result<()> {
        let path = self.path.clone();
        sqlite_task(move || {
            let connection = connection(&path)?;
            let sql = format!("INSERT INTO reference_sources ({SOURCE_COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)");
            connection.execute(&sql, params![source.id.to_string(),source.name,source.revision_label,serde_json::to_string(&source.provenance)?,source.host_path,source.manifest_digest.unwrap_or_default(),source.source_device.to_string(),source.source_inode.to_string(),source.state.to_string(),source.validation_error.as_ref().map(serde_json::to_string).transpose()?,source.file_count.to_string(),source.byte_count.to_string(),source.approved_for_agent_access,serde_json::to_string(&source.warnings)?,source.created_at.to_rfc3339(),source.updated_at.to_rfc3339()])?;
            Ok(())
        }).await
    }

    /// 验证 worker 只能完成仍在 validating 的登记项；删除后不会被迟到结果复活。
    pub async fn complete_reference_validation(&self, source: ReferenceSource) -> Result<bool> {
        let path = self.path.clone();
        sqlite_task(move || {
            let connection = connection(&path)?;
            Ok(connection.execute("UPDATE reference_sources SET provenance_json=?2,manifest_digest=?3,source_dev=?4,source_ino=?5,state=?6,error=?7,file_count=?8,byte_count=?9,warnings_json=?10,updated_at=?11 WHERE id=?1 AND state='validating'", params![source.id.to_string(),serde_json::to_string(&source.provenance)?,source.manifest_digest.unwrap_or_default(),source.source_device.to_string(),source.source_inode.to_string(),source.state.to_string(),source.validation_error.as_ref().map(serde_json::to_string).transpose()?,source.file_count.to_string(),source.byte_count.to_string(),serde_json::to_string(&source.warnings)?,source.updated_at.to_rfc3339()])? == 1)
        }).await
    }

    pub async fn begin_reference_validation(&self, source_id: Uuid) -> Result<()> {
        let path = self.path.clone();
        sqlite_task(move || {
            let connection = connection(&path)?;
            if connection.execute("UPDATE reference_sources SET state='validating',error=NULL,updated_at=?2 WHERE id=?1", params![source_id.to_string(),Utc::now().to_rfc3339()])? != 1 { return Err(conflict("source_not_found", "参考源不存在")); }
            Ok(())
        }).await
    }

    pub async fn patch_reference_source(
        &self,
        source_id: Uuid,
        patch: PatchReferenceSourceRequest,
    ) -> Result<ReferenceSource> {
        let path = self.path.clone();
        sqlite_task(move || {
            let mut connection = connection(&path)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut source = sources_on(&tx)?.into_iter().find(|source| source.id == source_id).ok_or_else(|| conflict("source_not_found", "参考源不存在"))?;
            if (patch.disabled == Some(true) || patch.approved_for_agent_access == Some(false)) && source_is_used(&tx,source_id)? { return Err(conflict("source_in_use", "参考源仍被项目配置、实际绑定或操作收据引用")); }
            if let Some(name) = patch.name { source.name = name; }
            if let Some(label) = patch.revision_label { source.revision_label = label; }
            if let Some(approved) = patch.approved_for_agent_access { source.approved_for_agent_access = approved; }
            if let Some(disabled) = patch.disabled {
                source.state = if disabled { SourceState::Disabled } else { SourceState::Validating };
            }
            source.updated_at = Utc::now();
            tx.execute("UPDATE reference_sources SET name=?2,revision_label=?3,approved_for_agent_access=?4,state=?5,updated_at=?6 WHERE id=?1", params![source_id.to_string(),source.name,source.revision_label,source.approved_for_agent_access,source.state.to_string(),source.updated_at.to_rfc3339()])?;
            tx.commit()?;
            Ok(source)
        }).await
    }

    pub async fn delete_reference_source(&self, source_id: Uuid) -> Result<()> {
        let path = self.path.clone();
        sqlite_task(move || {
            let mut connection = connection(&path)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if source_is_used(&tx, source_id)? {
                return Err(conflict("source_in_use", "参考源仍被引用"));
            }
            if tx.execute(
                "DELETE FROM reference_sources WHERE id=?1",
                [source_id.to_string()],
            )? != 1
            {
                return Err(conflict("source_not_found", "参考源不存在"));
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}
