use crate::*;

#[derive(Debug, Clone, toasty::Model)]
#[table = "mcp_servers"]
pub(crate) struct McpServerRecord {
    #[key]
    pub(crate) name: String,
    pub(crate) config_json: String,
    pub(crate) enabled: bool,
    pub(crate) sort_order: i64,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "settings"]
pub(crate) struct SettingRecord {
    #[key]
    pub(crate) key: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agents"]
pub(crate) struct AgentRecordRow {
    #[key]
    pub(crate) id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) project_id: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) container_id: Option<String>,
    pub(crate) docker_image: String,
    pub(crate) provider_id: String,
    pub(crate) provider_name: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) current_turn: Option<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) total_tokens: i64,
    pub(crate) system_prompt: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "projects"]
pub(crate) struct ProjectRecordRow {
    #[key]
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) repository_full_name: String,
    pub(crate) git_account_id: Option<String>,
    pub(crate) repository_id: i64,
    pub(crate) installation_id: i64,
    pub(crate) installation_account: String,
    pub(crate) branch: String,
    pub(crate) docker_image: String,
    pub(crate) clone_status: String,
    pub(crate) maintainer_agent_id: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) last_error: Option<String>,
    pub(crate) auto_review_enabled: bool,
    pub(crate) reviewer_extra_prompt: Option<String>,
    pub(crate) review_status: String,
    pub(crate) current_reviewer_agent_id: Option<String>,
    pub(crate) last_review_started_at: Option<String>,
    pub(crate) last_review_finished_at: Option<String>,
    pub(crate) next_review_at: Option<String>,
    pub(crate) last_review_outcome: Option<String>,
    pub(crate) review_last_error: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "tasks"]
pub(crate) struct TaskRecordRow {
    #[key]
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) planner_agent_id: String,
    pub(crate) current_agent_id: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) last_error: Option<String>,
    pub(crate) final_report: Option<String>,
    pub(crate) plan_status: String,
    pub(crate) plan_title: Option<String>,
    pub(crate) plan_markdown: Option<String>,
    pub(crate) plan_version: i64,
    pub(crate) plan_saved_by_agent_id: Option<String>,
    pub(crate) plan_saved_at: Option<String>,
    pub(crate) plan_approved_at: Option<String>,
    pub(crate) plan_revision_feedback: Option<String>,
    pub(crate) plan_revision_requested_at: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "task_reviews"]
pub(crate) struct TaskReviewRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) task_id: String,
    pub(crate) reviewer_agent_id: String,
    pub(crate) round: i64,
    pub(crate) passed: bool,
    pub(crate) findings: String,
    pub(crate) summary: String,
    pub(crate) created_at: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "project_review_runs"]
pub(crate) struct ProjectReviewRunRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) project_id: String,
    pub(crate) reviewer_agent_id: Option<String>,
    pub(crate) turn_id: Option<String>,
    #[index]
    pub(crate) started_at: String,
    pub(crate) finished_at: Option<String>,
    pub(crate) status: String,
    pub(crate) outcome: Option<String>,
    pub(crate) pr: Option<i64>,
    pub(crate) summary: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) messages_json: String,
    pub(crate) events_json: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "plan_history"]
pub(crate) struct PlanHistoryRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) task_id: String,
    pub(crate) version: i64,
    pub(crate) title: Option<String>,
    pub(crate) markdown: Option<String>,
    pub(crate) saved_at: Option<String>,
    pub(crate) saved_by_agent_id: Option<String>,
    pub(crate) revision_feedback: Option<String>,
    pub(crate) revision_requested_at: Option<String>,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_sessions"]
pub(crate) struct AgentSessionRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) agent_id: String,
    pub(crate) title: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_messages"]
pub(crate) struct AgentMessageRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) agent_id: String,
    #[index]
    pub(crate) session_id: String,
    pub(crate) position: i64,
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) created_at: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_history_items"]
pub(crate) struct AgentHistoryRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) agent_id: String,
    #[index]
    pub(crate) session_id: String,
    pub(crate) position: i64,
    pub(crate) item_json: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "service_events"]
pub(crate) struct ServiceEventRecord {
    #[key]
    pub(crate) sequence: i64,
    pub(crate) timestamp: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) event_json: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "agent_log_entries"]
pub(crate) struct AgentLogRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) agent_id: String,
    #[index]
    pub(crate) session_id: Option<String>,
    #[index]
    pub(crate) turn_id: Option<String>,
    pub(crate) level: String,
    #[index]
    pub(crate) category: String,
    pub(crate) message: String,
    pub(crate) details_json: String,
    #[index]
    pub(crate) timestamp: String,
}

#[derive(Debug, Clone, toasty::Model)]
#[table = "tool_trace_records"]
pub(crate) struct ToolTraceRecord {
    #[key]
    pub(crate) id: String,
    #[index]
    pub(crate) call_id: String,
    #[index]
    pub(crate) agent_id: String,
    #[index]
    pub(crate) session_id: Option<String>,
    #[index]
    pub(crate) turn_id: Option<String>,
    pub(crate) tool_name: String,
    pub(crate) arguments_json: String,
    pub(crate) output: String,
    pub(crate) success: bool,
    pub(crate) duration_ms: Option<i64>,
    #[index]
    pub(crate) started_at: String,
    pub(crate) completed_at: Option<String>,
    pub(crate) output_preview: String,
    pub(crate) output_artifacts_json: String,
}

impl AgentRecordRow {
    pub(crate) fn into_summary(self) -> Result<AgentSummary> {
        Ok(AgentSummary {
            id: parse_agent_id(&self.id)?,
            parent_id: self.parent_id.as_deref().map(parse_agent_id).transpose()?,
            task_id: self.task_id.as_deref().map(parse_task_id).transpose()?,
            project_id: self
                .project_id
                .as_deref()
                .map(parse_project_id)
                .transpose()?,
            role: self.role.as_deref().map(parse_store_enum).transpose()?,
            name: self.name,
            status: parse_store_enum(&self.status)?,
            container_id: self.container_id,
            docker_image: self.docker_image,
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            model: self.model,
            reasoning_effort: self.reasoning_effort,
            created_at: parse_utc(&self.created_at)?,
            updated_at: parse_utc(&self.updated_at)?,
            current_turn: self
                .current_turn
                .as_deref()
                .map(parse_turn_id)
                .transpose()?,
            last_error: self.last_error,
            token_usage: TokenUsage {
                input_tokens: i64_to_u64(self.input_tokens),
                output_tokens: i64_to_u64(self.output_tokens),
                total_tokens: i64_to_u64(self.total_tokens),
            },
        })
    }
}

impl ProjectRecordRow {
    pub(crate) fn into_summary(self) -> Result<ProjectSummary> {
        Ok(ProjectSummary {
            id: parse_project_id(&self.id)?,
            name: self.name,
            status: parse_store_enum(&self.status)?,
            owner: self.owner,
            repo: self.repo,
            repository_full_name: self.repository_full_name,
            git_account_id: self.git_account_id,
            repository_id: i64_to_u64(self.repository_id),
            installation_id: i64_to_u64(self.installation_id),
            installation_account: self.installation_account,
            branch: self.branch,
            docker_image: self.docker_image,
            clone_status: parse_store_enum(&self.clone_status)?,
            maintainer_agent_id: parse_agent_id(&self.maintainer_agent_id)?,
            created_at: parse_utc(&self.created_at)?,
            updated_at: parse_utc(&self.updated_at)?,
            last_error: self.last_error,
            auto_review_enabled: self.auto_review_enabled,
            reviewer_extra_prompt: self.reviewer_extra_prompt,
            review_status: parse_store_enum(&self.review_status)?,
            current_reviewer_agent_id: self
                .current_reviewer_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            last_review_started_at: self
                .last_review_started_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
            last_review_finished_at: self
                .last_review_finished_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
            next_review_at: self.next_review_at.as_deref().map(parse_utc).transpose()?,
            last_review_outcome: self
                .last_review_outcome
                .as_deref()
                .map(parse_store_enum)
                .transpose()?,
            review_last_error: self.review_last_error,
        })
    }
}

impl TaskRecordRow {
    pub(crate) fn into_persisted_task(
        self,
        reviews: Vec<TaskReview>,
        plan_history: Vec<PlanHistoryEntry>,
    ) -> Result<PersistedTask> {
        let plan = TaskPlan {
            status: parse_store_enum(&self.plan_status)?,
            title: self.plan_title,
            markdown: self.plan_markdown,
            version: i64_to_u64(self.plan_version),
            saved_by_agent_id: self
                .plan_saved_by_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            saved_at: self.plan_saved_at.as_deref().map(parse_utc).transpose()?,
            approved_at: self
                .plan_approved_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
            revision_feedback: self.plan_revision_feedback,
            revision_requested_at: self
                .plan_revision_requested_at
                .as_deref()
                .map(parse_utc)
                .transpose()?,
        };
        let summary = TaskSummary {
            id: parse_task_id(&self.id)?,
            title: self.title,
            status: parse_store_enum(&self.status)?,
            plan_status: plan.status.clone(),
            plan_version: plan.version,
            planner_agent_id: parse_agent_id(&self.planner_agent_id)?,
            current_agent_id: self
                .current_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            agent_count: 0,
            review_rounds: reviews.len() as u64,
            created_at: parse_utc(&self.created_at)?,
            updated_at: parse_utc(&self.updated_at)?,
            last_error: self.last_error,
            final_report: self.final_report,
        };
        Ok(PersistedTask {
            summary,
            plan,
            plan_history,
            reviews,
            artifacts: Vec::new(),
        })
    }
}

impl AgentMessageRecord {
    pub(crate) fn into_message(self) -> Result<AgentMessage> {
        Ok(AgentMessage {
            role: parse_store_enum(&self.role)?,
            content: self.content,
            created_at: parse_utc(&self.created_at)?,
        })
    }
}

impl TaskReviewRecord {
    pub(crate) fn into_review(self) -> Result<TaskReview> {
        Ok(TaskReview {
            id: parse_task_id(&self.id)?,
            task_id: parse_task_id(&self.task_id)?,
            reviewer_agent_id: parse_agent_id(&self.reviewer_agent_id)?,
            round: i64_to_u64(self.round),
            passed: self.passed,
            findings: self.findings,
            summary: self.summary,
            created_at: parse_utc(&self.created_at)?,
        })
    }
}

impl ProjectReviewRunRecord {
    pub(crate) fn into_summary(self) -> Result<ProjectReviewRunSummary> {
        Ok(ProjectReviewRunSummary {
            id: parse_uuid(&self.id)?,
            project_id: parse_project_id(&self.project_id)?,
            reviewer_agent_id: self
                .reviewer_agent_id
                .as_deref()
                .map(parse_agent_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            started_at: parse_utc(&self.started_at)?,
            finished_at: self.finished_at.as_deref().map(parse_utc).transpose()?,
            status: parse_store_enum(&self.status)?,
            outcome: self.outcome.as_deref().map(parse_store_enum).transpose()?,
            pr: self.pr.map(i64_to_u64),
            summary: self.summary,
            error: self.error,
        })
    }

    pub(crate) fn into_detail(self) -> Result<ProjectReviewRunDetail> {
        let messages = serde_json::from_str::<Vec<AgentMessage>>(&self.messages_json)?;
        let events = serde_json::from_str::<Vec<ServiceEvent>>(&self.events_json)?;
        Ok(ProjectReviewRunDetail {
            summary: self.into_summary()?,
            messages,
            events,
        })
    }
}

impl AgentLogRecord {
    pub(crate) fn into_entry(self) -> Result<AgentLogEntry> {
        Ok(AgentLogEntry {
            id: parse_uuid(&self.id)?,
            agent_id: parse_agent_id(&self.agent_id)?,
            session_id: self
                .session_id
                .as_deref()
                .map(parse_session_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            level: self.level,
            category: self.category,
            message: self.message,
            details: serde_json::from_str(&self.details_json)?,
            timestamp: parse_utc(&self.timestamp)?,
        })
    }
}

impl ToolTraceRecord {
    pub(crate) fn into_summary(self) -> Result<ToolTraceSummary> {
        Ok(ToolTraceSummary {
            call_id: self.call_id,
            agent_id: parse_agent_id(&self.agent_id)?,
            session_id: self
                .session_id
                .as_deref()
                .map(parse_session_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            tool_name: self.tool_name,
            success: self.success,
            started_at: parse_utc(&self.started_at)?,
            completed_at: self.completed_at.as_deref().map(parse_utc).transpose()?,
            duration_ms: self.duration_ms.map(i64_to_u64),
            output_preview: self.output_preview,
        })
    }

    pub(crate) fn into_detail(self) -> Result<ToolTraceDetail> {
        Ok(ToolTraceDetail {
            agent_id: parse_agent_id(&self.agent_id)?,
            session_id: self
                .session_id
                .as_deref()
                .map(parse_session_id)
                .transpose()?,
            turn_id: self.turn_id.as_deref().map(parse_turn_id).transpose()?,
            call_id: self.call_id,
            tool_name: self.tool_name,
            arguments: serde_json::from_str(&self.arguments_json)?,
            output: self.output,
            success: self.success,
            duration_ms: self.duration_ms.map(i64_to_u64),
            started_at: Some(parse_utc(&self.started_at)?),
            completed_at: self.completed_at.as_deref().map(parse_utc).transpose()?,
            output_preview: self.output_preview,
            output_artifacts: parse_tool_output_artifacts(&self.output_artifacts_json)?,
        })
    }
}

pub(crate) fn parse_tool_output_artifacts(value: &str) -> Result<Vec<ToolOutputArtifactInfo>> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(value)?)
}
