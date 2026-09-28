use crate::events::{next_product_event_sequence_on_path, recent_product_events_on_path};
use crate::records::*;
use crate::*;

impl MaiStore {
    pub async fn save_agent(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        crate::sqlite_busy::retry_sqlite_busy(|| async {
            self.save_agent_once(summary, system_prompt).await
        })
        .await
    }

    async fn save_agent_once(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_agent_row_in_tx(&mut tx, summary.id).await?;
        Query::<List<RetiredAgentSessionRecord>>::filter(
            RetiredAgentSessionRecord::fields()
                .agent_id()
                .eq(summary.id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        toasty::create!(AgentRecordRow {
            id: summary.id.to_string(),
            parent_id: summary.parent_id.map(|id| id.to_string()),
            task_id: summary.task_id.map(|id| id.to_string()),
            project_id: summary.project_id.map(|id| id.to_string()),
            role: summary.role.map(|r| r.to_string()),
            profile_id: summary.profile_id.clone(),
            workspace_json: summary
                .workspace
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            review_run_id: summary.review_run_id.map(|id| id.to_string()),
            name: summary.name.clone(),
            resource_state: summary.resource.state.to_string(),
            resource_error: summary.resource.error.clone(),
            container_id: summary.container_id.clone(),
            docker_image: summary.docker_image.clone(),
            provider_id: summary.provider_id.clone(),
            provider_name: summary.provider_name.clone(),
            model: summary.model.clone(),
            reasoning_effort: summary.reasoning_effort.clone(),
            created_at: summary.created_at.to_rfc3339(),
            updated_at: summary.updated_at.to_rfc3339(),
            system_prompt: system_prompt.map(str::to_string),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        crate::sqlite_busy::retry_sqlite_busy(|| async { self.delete_agent_once(agent_id).await })
            .await
    }

    async fn delete_agent_once(&self, agent_id: AgentId) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_agent_row_in_tx(&mut tx, agent_id).await?;
        Query::<List<RetiredAgentSessionRecord>>::filter(
            RetiredAgentSessionRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        toasty::create!(RetiredAgentSessionRecord {
            agent_id: agent_id.to_string(),
            retired_at: Utc::now().to_rfc3339(),
        })
        .exec(&mut tx)
        .await?;
        Query::<List<AgentLogRecord>>::filter(
            AgentLogRecord::fields().agent_id().eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<ToolTraceRecord>>::filter(
            ToolTraceRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 查询已到期且没有重新创建产品 Agent 的会话身份；不读取 PL 数据库。
    pub async fn expired_agent_sessions(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: usize,
    ) -> Result<Vec<AgentId>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<RetiredAgentSessionRecord>>::filter(
            RetiredAgentSessionRecord::fields()
                .retired_at()
                .lt(cutoff.to_rfc3339()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by(|left, right| left.retired_at.cmp(&right.retired_at));
        rows.into_iter()
            .take(batch_size)
            .map(|row| parse_agent_id(&row.agent_id))
            .collect()
    }

    /// PL 已完成删除后才移除产品清理收据，失败时下轮继续重试。
    pub async fn complete_agent_session_retirement(&self, agent_id: AgentId) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<RetiredAgentSessionRecord>>::filter(
            RetiredAgentSessionRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn load_runtime_snapshot(
        &self,
        recent_event_limit: usize,
    ) -> Result<RuntimeSnapshot> {
        let mut db = self.db.clone();
        let mut agent_rows = Query::<List<AgentRecordRow>>::all().exec(&mut db).await?;
        agent_rows.sort_by(|left, right| left.created_at.cmp(&right.created_at));

        let mut agents = Vec::with_capacity(agent_rows.len());
        for row in agent_rows {
            let system_prompt = row.system_prompt.clone();
            // mai-store 只加载 mai 产品 Agent/Task/Project；agent 的累计用量由产品
            // 运行时在内存中维护，不再从旧 pl-core Thread runtime 重建。
            let summary = row.into_summary()?;
            agents.push(PersistedAgent {
                summary,
                system_prompt,
            });
        }

        let mut task_rows = Query::<List<TaskRecordRow>>::all().exec(&mut db).await?;
        task_rows.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        let mut tasks = Vec::with_capacity(task_rows.len());
        for row in task_rows {
            let task_id = parse_task_id(&row.id)?;
            let reviews = self.load_task_reviews(task_id).await?;
            let plan_history = self.load_plan_history(task_id).await?;
            tasks.push(row.into_persisted_task(reviews, plan_history)?);
        }
        let projects = self.load_projects().await?;

        let next_sequence = next_product_event_sequence_on_path(&self.path).await?;
        let recent_events = recent_product_events_on_path(&self.path, recent_event_limit).await?;

        Ok(RuntimeSnapshot {
            agents,
            tasks,
            projects,
            recent_events,
            next_sequence,
        })
    }
}

pub(crate) async fn delete_agent_row_in_tx(
    tx: &mut toasty::Transaction<'_>,
    agent_id: AgentId,
) -> Result<()> {
    Query::<List<AgentRecordRow>>::filter(AgentRecordRow::fields().id().eq(agent_id.to_string()))
        .delete()
        .exec(tx)
        .await?;
    Ok(())
}
