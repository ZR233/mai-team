use super::*;

/// 存储层保留 Docker 资源收据的 JSON，不猜测或重建底层卷身份。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedReferenceOperation {
    pub operation: ReferenceApplyOperation,
    pub target_snapshot: AppliedReferenceSnapshot,
    pub old_mounts: Option<serde_json::Value>,
}

/// 唯一物理绑定事实；AgentSummary.container_id 只能是此记录的受控投影。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedReferenceBinding {
    pub agent_id: AgentId,
    pub project_id: Option<ProjectId>,
    pub generation: Uuid,
    pub container_id: String,
    pub effective_image_id: String,
    pub applied_revision: u64,
    pub fingerprint: String,
    pub snapshot: AppliedReferenceSnapshot,
    pub updated_at: DateTime<Utc>,
}

/// 应用意图的覆盖范围：配置更新补齐未应用环境，显式应用同时重新核验已应用环境。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceApplyScope {
    Unapplied,
    AllPhysicalEnvironments,
}

pub(super) fn enqueue_project_on(
    connection: &Connection,
    project_id: ProjectId,
    snapshot: &AppliedReferenceSnapshot,
    policy: ReferenceApplyPolicy,
    scope: ReferenceApplyScope,
) -> Result<Vec<Uuid>> {
    let mut query = connection.prepare("SELECT id,container_id,(SELECT applied_revision FROM agent_reference_bindings b WHERE b.agent_id=agents.id),(SELECT phase FROM reference_apply_operations r WHERE r.agent_id=agents.id ORDER BY created_at DESC,id DESC LIMIT 1) FROM agents WHERE project_id=?1 AND (container_id IS NOT NULL OR EXISTS(SELECT 1 FROM agent_reference_bindings b WHERE b.agent_id=agents.id))")?;
    let rows = query
        .query_map([project_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut ids = Vec::new();
    for (agent_id, container_id, applied_revision, latest_phase) in rows {
        if scope == ReferenceApplyScope::Unapplied
            && applied_revision
                .is_some_and(|revision| revision >= snapshot.configuration_revision as i64)
            && latest_phase.as_deref() != Some("failed")
        {
            continue;
        }
        let id = Uuid::new_v4();
        let timestamp = Utc::now().to_rfc3339();
        let changed = connection.execute("INSERT INTO reference_apply_operations(id,agent_id,project_id,target_revision,target_snapshot_json,policy,phase,old_container_id,created_at,updated_at) SELECT ?1,?2,?3,?4,?5,?6,'waiting_idle',?7,?8,?8 WHERE NOT EXISTS(SELECT 1 FROM reference_apply_operations WHERE agent_id=?2 AND phase NOT IN ('succeeded','failed','superseded'))", params![id.to_string(),agent_id,project_id.to_string(),snapshot.configuration_revision as i64,serde_json::to_string(snapshot)?,policy.to_string(),container_id,timestamp])?;
        if changed == 1 {
            ids.push(id);
        }
        if policy == ReferenceApplyPolicy::StopAndApply {
            connection.execute("UPDATE reference_apply_operations SET policy='stop_and_apply' WHERE agent_id=?1 AND phase NOT IN ('succeeded','failed','superseded')", [agent_id])?;
        }
    }
    Ok(ids)
}

fn operation_row(row: &Row<'_>) -> Result<PersistedReferenceOperation> {
    let id = |index| -> Result<Uuid> {
        Uuid::parse_str(&row.get::<_, String>(index)?)
            .map_err(|error| StoreError::DataIntegrity(error.to_string()))
    };
    let time = |index| -> Result<DateTime<Utc>> {
        Ok(DateTime::parse_from_rfc3339(&row.get::<_, String>(index)?)?.with_timezone(&Utc))
    };
    let failure: Option<String> = row.get(11)?;
    let mounts: Option<String> = row.get(10)?;
    Ok(PersistedReferenceOperation {
        operation: ReferenceApplyOperation {
            id: id(0)?,
            agent_id: id(1)?,
            project_id: id(2)?,
            target_revision: row.get::<_, i64>(3)? as u64,
            policy: ReferenceApplyPolicy::from_str(&row.get::<_, String>(5)?)?,
            phase: ReferenceApplyPhase::from_str(&row.get::<_, String>(6)?)?,
            old_container_id: row.get(7)?,
            candidate_container_id: row.get(8)?,
            snapshot_image_id: row.get(9)?,
            failure: failure.as_deref().map(serde_json::from_str).transpose()?,
            created_at: time(12)?,
            updated_at: time(13)?,
        },
        target_snapshot: serde_json::from_str(&row.get::<_, String>(4)?)?,
        old_mounts: mounts.as_deref().map(serde_json::from_str).transpose()?,
    })
}

impl MaiStore {
    pub async fn reference_operations(&self) -> Result<Vec<PersistedReferenceOperation>> {
        let path = self.path.clone();
        sqlite_task(move || {
            let connection = connection(&path)?;
            let mut statement = connection.prepare("SELECT id,agent_id,project_id,target_revision,target_snapshot_json,policy,phase,old_container_id,candidate_container_id,snapshot_image_id,old_mounts_json,error_json,created_at,updated_at FROM reference_apply_operations ORDER BY created_at,id")?;
            let mut rows = statement.query([])?; let mut operations = Vec::new();
            while let Some(row) = rows.next()? { operations.push(operation_row(row)?); }
            Ok(operations)
        }).await
    }

    pub async fn save_reference_operation(
        &self,
        receipt: PersistedReferenceOperation,
    ) -> Result<()> {
        let path = self.path.clone();
        sqlite_task(move || {
            let connection = connection(&path)?;
            let operation = receipt.operation;
            let changed = connection.execute("UPDATE reference_apply_operations SET phase=?2,policy=?3,old_container_id=?4,candidate_container_id=?5,snapshot_image_id=?6,old_mounts_json=?7,error_json=?8,updated_at=?9 WHERE id=?1 AND agent_id=?10", params![operation.id.to_string(),operation.phase.to_string(),operation.policy.to_string(),operation.old_container_id,operation.candidate_container_id,operation.snapshot_image_id,receipt.old_mounts.as_ref().map(serde_json::to_string).transpose()?,operation.failure.as_ref().map(serde_json::to_string).transpose()?,Utc::now().to_rfc3339(),operation.agent_id.to_string()])?;
            if changed != 1 { return Err(conflict("operation_superseded", "操作收据已不存在")); }
            Ok(())
        }).await
    }

    pub async fn reference_binding(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<PersistedReferenceBinding>> {
        let path = self.path.clone();
        sqlite_task(move || {
            let connection = connection(&path)?;
            let mut statement = connection.prepare("SELECT project_id,generation,container_id,effective_image_id,applied_revision,fingerprint,snapshot_json,updated_at FROM agent_reference_bindings WHERE agent_id=?1")?;
            let mut rows = statement.query([agent_id.to_string()])?;
            let Some(row) = rows.next()? else { return Ok(None); };
            let project: Option<String> = row.get(0)?;
            let parse_id = |value: &str| Uuid::parse_str(value).map_err(|error| StoreError::DataIntegrity(error.to_string()));
            Ok(Some(PersistedReferenceBinding { agent_id, project_id: project.as_deref().map(parse_id).transpose()?, generation: parse_id(&row.get::<_,String>(1)?)?, container_id: row.get(2)?, effective_image_id: row.get(3)?, applied_revision: row.get::<_,i64>(4)? as u64, fingerprint: row.get(5)?, snapshot: serde_json::from_str(&row.get::<_,String>(6)?)?, updated_at: DateTime::parse_from_rfc3339(&row.get::<_,String>(7)?)?.with_timezone(&Utc) }))
        }).await
    }

    /// 在目标修订与源身份 fence 通过后，同一事务更新实际绑定和摘要容器投影。
    pub async fn publish_reference_binding(
        &self,
        binding: PersistedReferenceBinding,
    ) -> Result<()> {
        let path = self.path.clone();
        sqlite_task(move || {
            let mut connection = connection(&path)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let agent_project = tx.query_row("SELECT project_id FROM agents WHERE id=?1", [binding.agent_id.to_string()], |row| row.get::<_, Option<String>>(0)).optional()?
                .ok_or_else(|| conflict("agent_deleted", "Agent 已被删除"))?;
            if agent_project != binding.project_id.map(|id| id.to_string()) { return Err(conflict("project_not_found", "Agent 的可信项目身份与环境绑定不一致")); }
            if let Some(project_id) = binding.project_id {
                let status = tx.query_row("SELECT status FROM projects WHERE id=?1", [project_id.to_string()], |row| row.get::<_, String>(0)).optional()?
                    .ok_or_else(|| conflict("project_not_found", "项目已被删除"))?;
                if status == "deleting" { return Err(conflict("project_deleting", "项目正在删除，不能发布执行环境")); }
                let current = super::configurations::configuration_on(&tx,project_id)?;
                if current.revision != binding.applied_revision { return Err(conflict("revision_conflict", "发布前项目修订号已改变")); }
                let sources = super::sources::sources_on(&tx)?;
                for applied in &binding.snapshot.references {
                    let valid = sources.iter().any(|source| source.id == applied.source_id && source.state == SourceState::Ready && source.approved_for_agent_access && source.host_path == applied.host_path && source.source_device == applied.source_device && source.source_inode == applied.source_inode && source.manifest_digest.as_deref() == Some(applied.tree_digest.as_str()));
                    if !valid { return Err(conflict("source_identity_changed", "发布前参考源身份失效")); }
                }
            }
            tx.execute("INSERT INTO agent_reference_bindings(agent_id,project_id,generation,container_id,effective_image_id,applied_revision,fingerprint,snapshot_json,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(agent_id) DO UPDATE SET project_id=excluded.project_id,generation=excluded.generation,container_id=excluded.container_id,effective_image_id=excluded.effective_image_id,applied_revision=excluded.applied_revision,fingerprint=excluded.fingerprint,snapshot_json=excluded.snapshot_json,updated_at=excluded.updated_at", params![binding.agent_id.to_string(),binding.project_id.map(|id| id.to_string()),binding.generation.to_string(),binding.container_id,binding.effective_image_id,binding.applied_revision as i64,binding.fingerprint,serde_json::to_string(&binding.snapshot)?,binding.updated_at.to_rfc3339()])?;
            if tx.execute("UPDATE agents SET container_id=?2,updated_at=?3 WHERE id=?1", params![binding.agent_id.to_string(),binding.container_id,binding.updated_at.to_rfc3339()])? != 1 { return Err(conflict("agent_deleted", "Agent 已被删除")); }
            tx.commit()?;
            Ok(())
        }).await
    }
}
