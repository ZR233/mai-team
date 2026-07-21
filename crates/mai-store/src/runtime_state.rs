use crate::events::{next_product_event_sequence_on_path, recent_product_events_on_path};
use crate::records::*;
use crate::*;

impl MaiStore {
    pub async fn save_agent(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        self.save_agent_with_runtime_id(summary, system_prompt, &summary.id.to_string())
            .await
    }

    pub async fn save_agent_with_runtime_id(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
        runtime_agent_id: &str,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_agent_row_in_tx(&mut tx, summary.id).await?;
        toasty::create!(AgentRecordRow {
            id: summary.id.to_string(),
            runtime_agent_id: runtime_agent_id.to_string(),
            parent_id: summary.parent_id.map(|id| id.to_string()),
            task_id: summary.task_id.map(|id| id.to_string()),
            project_id: summary.project_id.map(|id| id.to_string()),
            role: summary.role.map(|r| r.to_string()),
            name: summary.name.clone(),
            resource_state: summary.state.resource.to_string(),
            resource_error: summary.state.resource_error.clone(),
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
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        let runtime_agent_id = Query::<List<AgentRecordRow>>::filter(
            AgentRecordRow::fields().id().eq(agent_id.to_string()),
        )
        .exec(&mut tx)
        .await?
        .into_iter()
        .next()
        .map(|row| row.runtime_agent_id);
        delete_agent_row_in_tx(&mut tx, agent_id).await?;
        if let Some(runtime_agent_id) = runtime_agent_id {
            delete_agent_runtime_in_tx(&mut tx, &runtime_agent_id).await?;
        }
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
            let runtime_agent_id = row.runtime_agent_id.clone();
            let mut summary = row.into_summary()?;
            if let Some(runtime) = self.load_agent_runtime(&runtime_agent_id).await? {
                for session in runtime.sessions {
                    summary.token_usage.add(&TokenUsage {
                        input_tokens: session.usage.prompt_tokens,
                        cached_input_tokens: session.usage.cached_prompt_tokens,
                        output_tokens: session.usage.completion_tokens,
                        reasoning_output_tokens: session.usage.reasoning_tokens,
                        total_tokens: session.usage.total_tokens,
                    });
                }
            }
            agents.push(PersistedAgent {
                summary,
                system_prompt,
                runtime_agent_id,
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

async fn delete_agent_runtime_in_tx(
    tx: &mut toasty::Transaction<'_>,
    runtime_agent_id: &str,
) -> Result<()> {
    Query::<List<AgentRuntimeStateRecord>>::filter(
        AgentRuntimeStateRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentSessionRecord>>::filter(
        AgentSessionRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentHistoryRecord>>::filter(
        AgentHistoryRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentMessageRecord>>::filter(
        AgentMessageRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentPendingInputRecord>>::filter(
        AgentPendingInputRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentTurnRecord>>::filter(
        AgentTurnRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentRuntimeEventRecord>>::filter(
        AgentRuntimeEventRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<AgentRuntimeTraceRecord>>::filter(
        AgentRuntimeTraceRecord::fields()
            .agent_id()
            .eq(runtime_agent_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Ok(())
}
