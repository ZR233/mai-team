use super::*;

pub(super) fn configuration_on(
    connection: &Connection,
    project_id: ProjectId,
) -> Result<ProjectReferenceConfiguration> {
    let row = connection
        .query_row(
            "SELECT revision,bindings_json FROM project_reference_configs WHERE project_id=?1",
            [project_id.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let (revision, json) =
        row.ok_or_else(|| conflict("project_not_found", "项目引用配置不存在"))?;
    Ok(ProjectReferenceConfiguration {
        revision: u64::try_from(revision)
            .map_err(|error| StoreError::DataIntegrity(error.to_string()))?,
        bindings: serde_json::from_str(&json)?,
    })
}

impl MaiStore {
    /// 调用方在项目锁内读取登记清单，以便 gate 覆盖 CAS 提交之前已存在的全部 Thread。
    pub async fn reference_agent_inventory(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<(AgentId, bool)>> {
        let path = self.path.clone();
        sqlite_task(move || {
            let connection = connection(&path)?;
            let mut statement = connection.prepare("SELECT id, container_id IS NOT NULL OR EXISTS(SELECT 1 FROM agent_reference_bindings b WHERE b.agent_id=agents.id) FROM agents WHERE project_id=?1 ORDER BY id")?;
            let rows = statement.query_map([project_id.to_string()], |row| Ok((row.get::<_,String>(0)?, row.get::<_,bool>(1)?)))?;
            rows.map(|row| { let (id, physical) = row?; Ok((Uuid::parse_str(&id).map_err(|error| StoreError::DataIntegrity(error.to_string()))?, physical)) }).collect()
        }).await
    }

    pub async fn project_reference_configuration(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectReferenceConfiguration> {
        let path = self.path.clone();
        sqlite_task(move || configuration_on(&connection(&path)?, project_id)).await
    }

    /// CAS、配置及各 Agent 的待应用意图在同一事务提交，事件通知不承担恢复责任。
    pub async fn replace_project_reference_configuration(
        &self,
        project_id: ProjectId,
        request: ReplaceProjectReferencesRequest,
        snapshot: AppliedReferenceSnapshot,
    ) -> Result<ProjectReferenceConfiguration> {
        let path = self.path.clone();
        sqlite_task(move || {
            let mut connection = connection(&path)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let status: String = tx.query_row("SELECT status FROM projects WHERE id=?1", [project_id.to_string()], |row| row.get(0)).optional()?.ok_or_else(|| conflict("project_not_found", "项目不存在"))?;
            if status == "deleting" { return Err(conflict("project_deleting", "项目正在删除")); }
            let current = configuration_on(&tx,project_id)?;
            if request.expected_revision != current.revision { return Err(conflict("revision_conflict",format!("当前修订号为 {}",current.revision))); }
            if current.bindings == request.bindings { tx.commit()?; return Ok(current); }
            let revision = current.revision.checked_add(1).filter(|revision| *revision <= MAX_JS_SAFE_INTEGER).ok_or_else(|| conflict("revision_not_js_safe", "修订号超出安全整数范围"))?;
            if snapshot.project_id != project_id || snapshot.configuration_revision != revision { return Err(StoreError::DataIntegrity("目标快照与 CAS 修订号不一致".into())); }
            let sources = sources::sources_on(&tx)?;
            for binding in request.bindings.iter().filter(|binding| binding.enabled) {
                let source = sources.iter().find(|source| source.id == binding.source_id).ok_or_else(|| conflict("source_not_found", "启用的参考源不存在"))?;
                if source.state != SourceState::Ready || !source.approved_for_agent_access { return Err(conflict("source_not_ready", "启用的参考源尚未就绪或未经批准")); }
                let applied = snapshot.references.iter().find(|reference| reference.binding_id == binding.id).ok_or_else(|| StoreError::DataIntegrity("目标快照遗漏启用条目".into()))?;
                if applied.host_path != source.host_path || applied.tree_digest != source.manifest_digest.as_deref().unwrap_or("") || applied.source_device != source.source_device || applied.source_inode != source.source_inode { return Err(conflict("source_identity_changed", "参考源身份在保存前发生变化")); }
            }
            let configuration = ProjectReferenceConfiguration { revision, bindings: request.bindings };
            tx.execute("UPDATE project_reference_configs SET revision=?2,bindings_json=?3,updated_at=?4 WHERE project_id=?1", params![project_id.to_string(),revision as i64,serde_json::to_string(&configuration.bindings)?,Utc::now().to_rfc3339()])?;
            operations::enqueue_project_on(&tx,project_id,&snapshot,ReferenceApplyPolicy::WhenIdle, ReferenceApplyScope::Unapplied)?;
            tx.commit()?;
            Ok(configuration)
        }).await
    }

    pub async fn enqueue_reference_application(
        &self,
        project_id: ProjectId,
        snapshot: AppliedReferenceSnapshot,
        policy: ReferenceApplyPolicy,
        scope: ReferenceApplyScope,
    ) -> Result<Vec<Uuid>> {
        let path = self.path.clone();
        sqlite_task(move || {
            let mut connection = connection(&path)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = configuration_on(&tx, project_id)?;
            if current.revision != snapshot.configuration_revision {
                return Err(conflict("revision_conflict", "配置修订号已变化"));
            }
            let status: String = tx.query_row(
                "SELECT status FROM projects WHERE id=?1",
                [project_id.to_string()],
                |row| row.get(0),
            )?;
            if status == "deleting" {
                return Err(conflict("project_deleting", "项目正在删除，不能开始应用"));
            }
            let ids = operations::enqueue_project_on(&tx, project_id, &snapshot, policy, scope)?;
            tx.commit()?;
            Ok(ids)
        })
        .await
    }
}
