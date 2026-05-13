use crate::records::*;
use crate::*;

impl ConfigStore {
    pub async fn append_agent_log_entry(&self, entry: &AgentLogEntry) -> Result<()> {
        let mut db = self.db.clone();
        toasty::create!(AgentLogRecord {
            id: entry.id.to_string(),
            agent_id: entry.agent_id.to_string(),
            session_id: entry.session_id.map(|id| id.to_string()),
            turn_id: entry.turn_id.map(|id| id.to_string()),
            level: entry.level.clone(),
            category: entry.category.clone(),
            message: entry.message.clone(),
            details_json: serde_json::to_string(&entry.details)?,
            timestamp: entry.timestamp.to_rfc3339(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn list_agent_logs(
        &self,
        agent_id: AgentId,
        filter: AgentLogFilter,
    ) -> Result<Vec<AgentLogEntry>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<AgentLogRecord>>::filter(
            AgentLogRecord::fields().agent_id().eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        if let Some(session_id) = filter.session_id {
            let session_id = session_id.to_string();
            rows.retain(|row| row.session_id.as_deref() == Some(session_id.as_str()));
        }
        if let Some(turn_id) = filter.turn_id {
            let turn_id = turn_id.to_string();
            rows.retain(|row| row.turn_id.as_deref() == Some(turn_id.as_str()));
        }
        if let Some(level) = filter.level {
            rows.retain(|row| row.level == level);
        }
        if let Some(category) = filter.category {
            rows.retain(|row| row.category == category);
        }
        if let Some(since) = filter.since {
            let since = since.to_rfc3339();
            rows.retain(|row| row.timestamp >= since);
        }
        if let Some(until) = filter.until {
            let until = until.to_rfc3339();
            rows.retain(|row| row.timestamp <= until);
        }
        rows.sort_by(|left, right| {
            right
                .timestamp
                .cmp(&left.timestamp)
                .then_with(|| right.id.cmp(&left.id))
        });
        rows.into_iter()
            .skip(filter.offset)
            .take(filter.limit.max(1))
            .map(AgentLogRecord::into_entry)
            .collect()
    }

    pub async fn prune_agent_logs_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<AgentLogRecord>>::all().exec(&mut db).await?;
        let old_ids = rows
            .into_iter()
            .filter(|row| row.timestamp < cutoff)
            .map(|row| row.id)
            .collect::<Vec<_>>();
        for id in &old_ids {
            Query::<List<AgentLogRecord>>::filter(AgentLogRecord::fields().id().eq(id.clone()))
                .delete()
                .exec(&mut db)
                .await?;
        }
        Ok(old_ids.len())
    }

    pub async fn save_tool_trace_started(
        &self,
        trace: &ToolTraceDetail,
        started_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        delete_matching_tool_trace(&mut db, trace).await?;
        toasty::create!(ToolTraceRecord {
            id: tool_trace_record_id(trace),
            call_id: trace.call_id.clone(),
            agent_id: trace.agent_id.to_string(),
            session_id: trace.session_id.map(|id| id.to_string()),
            turn_id: trace.turn_id.map(|id| id.to_string()),
            tool_name: trace.tool_name.clone(),
            arguments_json: serde_json::to_string(&trace.arguments)?,
            output: String::new(),
            success: false,
            duration_ms: None,
            started_at: started_at.to_rfc3339(),
            completed_at: None,
            output_preview: String::new(),
            output_artifacts_json: "[]".to_string(),
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn save_tool_trace_completed(
        &self,
        trace: &ToolTraceDetail,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut db = self.db.clone();
        delete_matching_tool_trace(&mut db, trace).await?;
        toasty::create!(ToolTraceRecord {
            id: tool_trace_record_id(trace),
            call_id: trace.call_id.clone(),
            agent_id: trace.agent_id.to_string(),
            session_id: trace.session_id.map(|id| id.to_string()),
            turn_id: trace.turn_id.map(|id| id.to_string()),
            tool_name: trace.tool_name.clone(),
            arguments_json: serde_json::to_string(&trace.arguments)?,
            output: trace.output.clone(),
            success: trace.success,
            duration_ms: trace.duration_ms.map(u64_to_i64),
            started_at: started_at.to_rfc3339(),
            completed_at: Some(completed_at.to_rfc3339()),
            output_preview: trace.output_preview.clone(),
            output_artifacts_json: serde_json::to_string(&trace.output_artifacts)?,
        })
        .exec(&mut db)
        .await?;
        Ok(())
    }

    pub async fn load_tool_trace(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        call_id: &str,
    ) -> Result<Option<ToolTraceDetail>> {
        let mut db = self.db.clone();
        let rows = Query::<List<ToolTraceRecord>>::filter(
            ToolTraceRecord::fields().call_id().eq(call_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        rows.into_iter()
            .find(|row| {
                tool_trace_belongs_to(
                    row,
                    agent_id,
                    session_id.map(|id| id.to_string()).as_deref(),
                    None,
                )
            })
            .map(ToolTraceRecord::into_detail)
            .transpose()
    }

    pub async fn list_tool_traces(
        &self,
        agent_id: AgentId,
        filter: ToolTraceFilter,
    ) -> Result<Vec<ToolTraceSummary>> {
        let mut db = self.db.clone();
        let mut rows = Query::<List<ToolTraceRecord>>::filter(
            ToolTraceRecord::fields()
                .agent_id()
                .eq(agent_id.to_string()),
        )
        .exec(&mut db)
        .await?;
        if let Some(session_id) = filter.session_id {
            let session_id = session_id.to_string();
            rows.retain(|row| row.session_id.as_deref() == Some(session_id.as_str()));
        }
        if let Some(turn_id) = filter.turn_id {
            let turn_id = turn_id.to_string();
            rows.retain(|row| row.turn_id.as_deref() == Some(turn_id.as_str()));
        }
        rows.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.call_id.cmp(&left.call_id))
        });
        rows.into_iter()
            .skip(filter.offset)
            .take(filter.limit.max(1))
            .map(ToolTraceRecord::into_summary)
            .collect()
    }

    pub async fn prune_tool_traces_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let mut db = self.db.clone();
        let cutoff = cutoff.to_rfc3339();
        let rows = Query::<List<ToolTraceRecord>>::all().exec(&mut db).await?;
        let old_ids = rows
            .into_iter()
            .filter(|row| row.started_at < cutoff)
            .map(|row| row.id)
            .collect::<Vec<_>>();
        for id in &old_ids {
            Query::<List<ToolTraceRecord>>::filter(ToolTraceRecord::fields().id().eq(id.clone()))
                .delete()
                .exec(&mut db)
                .await?;
        }
        Ok(old_ids.len())
    }
}

async fn delete_matching_tool_trace(db: &mut Db, trace: &ToolTraceDetail) -> Result<()> {
    let rows = Query::<List<ToolTraceRecord>>::filter(
        ToolTraceRecord::fields()
            .call_id()
            .eq(trace.call_id.clone()),
    )
    .exec(db)
    .await?;
    let session_id = trace.session_id.map(|id| id.to_string());
    let turn_id = trace.turn_id.map(|id| id.to_string());
    for row in rows {
        if tool_trace_belongs_to(
            &row,
            trace.agent_id,
            session_id.as_deref(),
            turn_id.as_deref(),
        ) {
            Query::<List<ToolTraceRecord>>::filter(ToolTraceRecord::fields().id().eq(row.id))
                .delete()
                .exec(db)
                .await?;
        }
    }
    Ok(())
}

fn tool_trace_record_id(trace: &ToolTraceDetail) -> String {
    format!(
        "{}:{}:{}:{}",
        trace.agent_id,
        trace
            .session_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        trace.turn_id.map(|id| id.to_string()).unwrap_or_default(),
        trace.call_id
    )
}

fn tool_trace_belongs_to(
    row: &ToolTraceRecord,
    agent_id: AgentId,
    session_id: Option<&str>,
    turn_id: Option<&str>,
) -> bool {
    row.agent_id == agent_id.to_string()
        && session_id.is_none_or(|id| row.session_id.as_deref() == Some(id))
        && turn_id.is_none_or(|id| row.turn_id.as_deref() == Some(id))
}
