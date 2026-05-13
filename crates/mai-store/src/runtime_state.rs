use crate::records::*;
use crate::settings::delete_setting_in_tx;
use crate::*;

impl ConfigStore {
    pub async fn save_agent(
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
            status: summary.status.to_string(),
            container_id: summary.container_id.clone(),
            docker_image: summary.docker_image.clone(),
            provider_id: summary.provider_id.clone(),
            provider_name: summary.provider_name.clone(),
            model: summary.model.clone(),
            reasoning_effort: summary.reasoning_effort.clone(),
            created_at: summary.created_at.to_rfc3339(),
            updated_at: summary.updated_at.to_rfc3339(),
            current_turn: summary.current_turn.map(|id| id.to_string()),
            last_error: summary.last_error.clone(),
            input_tokens: u64_to_i64(summary.token_usage.input_tokens),
            output_tokens: u64_to_i64(summary.token_usage.output_tokens),
            total_tokens: u64_to_i64(summary.token_usage.total_tokens),
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
        delete_agent_row_in_tx(&mut tx, agent_id).await?;
        Query::<List<AgentSessionRecord>>::filter(
            AgentSessionRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<AgentMessageRecord>>::filter(
            AgentMessageRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
        .exec(&mut tx)
        .await?;
        Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .delete()
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

    pub async fn save_agent_session(
        &self,
        agent_id: AgentId,
        session: &AgentSessionSummary,
    ) -> Result<()> {
        let mut db = self.db.clone();
        Query::<List<AgentSessionRecord>>::filter(
            AgentSessionRecord::fields().id().eq(session.id.to_string()),
        )
        .delete()
        .exec(&mut db)
        .await?;
        toasty::create!(AgentSessionRecord {
            id: session.id.to_string(),
            agent_id: agent_id.to_string(),
            title: session.title.clone(),
            created_at: session.created_at.to_rfc3339(),
            updated_at: session.updated_at.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn append_agent_message(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        position: usize,
        message: &AgentMessage,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentMessageRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            position: position as i64,
            role: message.role.to_string(),
            content: message.content.clone(),
            created_at: message.created_at.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn append_agent_history_item(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        position: usize,
        item: &ModelInputItem,
    ) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentHistoryRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            position: position as i64,
            item_json: serde_json::to_string(item)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn replace_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        items: &[ModelInputItem],
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields()
                .agent_id()
                .eq(agent_id.to_string())
                .and(
                    AgentHistoryRecord::fields()
                        .session_id()
                        .eq(session_id.to_string()),
                ),
        )
        .delete()
        .exec(&mut tx)
        .await?;

        for (position, item) in items.iter().enumerate() {
            toasty::create!(AgentHistoryRecord {
                id: Uuid::new_v4().to_string(),
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                position: position as i64,
                item_json: serde_json::to_string(item)?,
            })
            .exec(&mut tx)
            .await?;
        }
        delete_setting_in_tx(&mut tx, &session_context_tokens_key(agent_id, session_id)).await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn save_session_context_tokens(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        tokens: u64,
    ) -> Result<()> {
        self.set_setting(
            &session_context_tokens_key(agent_id, session_id),
            &tokens.to_string(),
        )
        .await
    }

    pub async fn clear_session_context_tokens(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<()> {
        let mut db = self.db.clone();
        let mut tx = db.transaction().await?;
        delete_setting_in_tx(&mut tx, &session_context_tokens_key(agent_id, session_id)).await?;
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
            let agent_id = parse_agent_id(&row.id)?;
            let sessions = self.load_agent_sessions(agent_id).await?;
            let system_prompt = row.system_prompt.clone();
            agents.push(PersistedAgent {
                summary: row.into_summary()?,
                sessions,
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

        let mut events = Query::<List<ServiceEventRecord>>::all()
            .exec(&mut db)
            .await?;
        events.sort_by_key(|event| event.sequence);
        let next_sequence = events
            .last()
            .map(|event| i64_to_u64(event.sequence).saturating_add(1))
            .unwrap_or(1);
        let skip = events.len().saturating_sub(recent_event_limit);
        let recent_events = events
            .into_iter()
            .skip(skip)
            .map(|row| serde_json::from_str::<ServiceEvent>(&row.event_json).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;

        Ok(RuntimeSnapshot {
            agents,
            tasks,
            projects,
            recent_events,
            next_sequence,
        })
    }

    pub(crate) async fn load_agent_sessions(
        &self,
        agent_id: AgentId,
    ) -> Result<Vec<PersistedAgentSession>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentSessionRecord>>::filter(
            AgentSessionRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });

        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            let session_id = parse_session_id(&row.id)?;
            let messages = self.load_agent_messages(agent_id, session_id).await?;
            let history = self.load_agent_history(agent_id, session_id).await?;
            let last_context_tokens = self
                .load_session_context_tokens(agent_id, session_id)
                .await?;
            sessions.push(PersistedAgentSession {
                summary: AgentSessionSummary {
                    id: session_id,
                    title: row.title,
                    created_at: parse_utc(&row.created_at)?,
                    updated_at: parse_utc(&row.updated_at)?,
                    message_count: messages.len(),
                },
                history,
                last_context_tokens,
            });
        }
        Ok(sessions)
    }

    pub async fn load_agent_messages(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Vec<AgentMessage>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentMessageRecord>>::filter(
            AgentMessageRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.retain(|row| row.session_id == session_id.to_string());
        rows.sort_by_key(|row| row.position);
        rows.into_iter()
            .map(AgentMessageRecord::into_message)
            .collect()
    }

    pub(crate) async fn load_agent_history(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Vec<ModelInputItem>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentHistoryRecord>>::filter(
            AgentHistoryRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.retain(|row| row.session_id == session_id.to_string());
        rows.sort_by_key(|row| row.position);
        rows.into_iter()
            .map(|row| serde_json::from_str::<ModelInputItem>(&row.item_json).map_err(Into::into))
            .collect()
    }

    async fn load_session_context_tokens(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<Option<u64>> {
        Ok(self
            .get_setting(&session_context_tokens_key(agent_id, session_id))
            .await?
            .and_then(|value| value.parse::<u64>().ok()))
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
