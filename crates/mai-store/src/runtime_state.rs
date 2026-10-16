use crate::events::{next_product_event_sequence_on_path, recent_product_events_on_path};
use crate::records::*;
use crate::*;

impl MaiStore {
    /// 创建一条新的产品 Agent 行。
    ///
    /// 只负责创建：ID 已存在或该 ID 仍在退休流程里都显式失败；普通更新必须走
    /// [`MaiStore::save_agent`]，那条路径不会复活已删除的父行。
    pub async fn insert_agent(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        crate::sqlite_busy::retry_sqlite_busy(|| async {
            self.insert_agent_once(summary, system_prompt).await
        })
        .await
    }

    async fn insert_agent_once(
        &self,
        summary: &AgentSummary,
        system_prompt: Option<&str>,
    ) -> Result<()> {
        crate::parent_saves::insert_agent_row(self, summary, system_prompt).await
    }

    /// 更新一条已经存在的产品 Agent 行。
    ///
    /// 目标行不存在（例如已经被删除）时显式失败：普通保存既不会复活父行，也不会清掉
    /// `retired_agent_sessions` 的墓碑。
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
        crate::parent_saves::save_agent_row(self, summary, system_prompt).await
    }

    pub async fn delete_agent(&self, agent_id: AgentId) -> Result<()> {
        crate::sqlite_busy::retry_sqlite_busy(|| async { self.delete_agent_once(agent_id).await })
            .await
    }

    async fn delete_agent_once(&self, agent_id: AgentId) -> Result<()> {
        crate::resource_deletion::delete_agent(self, agent_id).await
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
