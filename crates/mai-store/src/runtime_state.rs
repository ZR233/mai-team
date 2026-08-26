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
        toasty::create!(AgentRecordRow {
            id: summary.id.to_string(),
            parent_id: summary.parent_id.map(|id| id.to_string()),
            task_id: summary.task_id.map(|id| id.to_string()),
            project_id: summary.project_id.map(|id| id.to_string()),
            role: summary.role.map(|r| r.to_string()),
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
        delete_thread_runtime_in_tx(&mut tx, &agent_id.to_string()).await?;
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
            let mut summary = row.into_summary()?;
            if let Some(runtime) = self.load_thread_runtime(&summary.id.to_string()).await?
                && let Some(usage) = runtime.snapshot.and_then(|snapshot| snapshot.runtime)
            {
                summary.token_usage.prompt_tokens = summary
                    .token_usage
                    .prompt_tokens
                    .saturating_add(usage.usage.prompt_tokens);
                summary.token_usage.cached_prompt_tokens = summary
                    .token_usage
                    .cached_prompt_tokens
                    .saturating_add(usage.usage.cached_prompt_tokens);
                summary.token_usage.cache_write_tokens = summary
                    .token_usage
                    .cache_write_tokens
                    .saturating_add(usage.usage.cache_write_tokens);
                summary.token_usage.completion_tokens = summary
                    .token_usage
                    .completion_tokens
                    .saturating_add(usage.usage.completion_tokens);
                summary.token_usage.reasoning_tokens = summary
                    .token_usage
                    .reasoning_tokens
                    .saturating_add(usage.usage.reasoning_tokens);
                summary.token_usage.total_tokens = summary
                    .token_usage
                    .total_tokens
                    .saturating_add(usage.usage.total_tokens);
            }
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

async fn delete_thread_runtime_in_tx(
    tx: &mut toasty::Transaction<'_>,
    thread_id: &str,
) -> Result<()> {
    Query::<List<ThreadRuntimeDocumentRecord>>::filter(
        ThreadRuntimeDocumentRecord::fields()
            .thread_id()
            .eq(thread_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<ThreadTurnRecord>>::filter(
        ThreadTurnRecord::fields()
            .thread_id()
            .eq(thread_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<ThreadItemRecord>>::filter(
        ThreadItemRecord::fields()
            .thread_id()
            .eq(thread_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<ThreadNotificationRecord>>::filter(
        ThreadNotificationRecord::fields()
            .thread_id()
            .eq(thread_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<ThreadRuntimeEventRecord>>::filter(
        ThreadRuntimeEventRecord::fields()
            .thread_id()
            .eq(thread_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<ThreadRuntimeTraceRecord>>::filter(
        ThreadRuntimeTraceRecord::fields()
            .thread_id()
            .eq(thread_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Query::<List<ThreadSubmissionRecord>>::filter(
        ThreadSubmissionRecord::fields()
            .thread_id()
            .eq(thread_id.to_string()),
    )
    .delete()
    .exec(&mut *tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn deleting_thread_runtime_removes_durable_submissions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = MaiStore::open_with_config_and_artifact_index_path(
            directory.path().join("store.sqlite3"),
            directory.path().join("config.toml"),
            directory.path().join("artifacts"),
        )
        .await
        .expect("open store");
        let thread_id = "reviewer-thread";
        let mut db = store.db.clone();
        toasty::create!(ThreadSubmissionRecord {
            id: format!("{thread_id}:1"),
            thread_id: thread_id.to_string(),
            ordinal: 1,
            created_at: 1,
            submission_json: "{}".to_string(),
        })
        .exec(&mut db)
        .await
        .expect("save submission");

        let mut tx = db.transaction().await.expect("begin delete");
        delete_thread_runtime_in_tx(&mut tx, thread_id)
            .await
            .expect("delete runtime");
        tx.commit().await.expect("commit delete");

        assert_eq!(
            store
                .list_thread_submissions(thread_id, 0, 20)
                .await
                .expect("list submissions")
                .items,
            Vec::new()
        );
    }
}
