//! 只删除明确归属的产品元数据；宿主源码及 Docker 资源不属于存储层。
use crate::parent_saves::{connection, sqlite_task};
use crate::*;
use rusqlite::params;

pub(crate) async fn delete_agent(store: &MaiStore, agent_id: AgentId) -> Result<()> {
    let path = store.path.clone();
    sqlite_task(move || {
        let mut connection = connection(&path)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = agent_id.to_string();
        tx.execute("DELETE FROM reference_apply_operations WHERE agent_id=?1", [&id])?;
        tx.execute("DELETE FROM agent_reference_bindings WHERE agent_id=?1", [&id])?;
        tx.execute("DELETE FROM agents WHERE id=?1", [&id])?;
        tx.execute("INSERT INTO retired_agent_sessions(agent_id,retired_at) VALUES(?1,?2) ON CONFLICT(agent_id) DO UPDATE SET retired_at=excluded.retired_at", params![id,Utc::now().to_rfc3339()])?;
        tx.execute("DELETE FROM agent_log_entries WHERE agent_id=?1", [&id])?;
        tx.execute("DELETE FROM tool_trace_records WHERE agent_id=?1", [&id])?;
        tx.commit()?;
        Ok(())
    }).await
}

pub(crate) async fn delete_project(store: &MaiStore, project_id: ProjectId) -> Result<()> {
    let path = store.path.clone();
    sqlite_task(move || {
        let mut connection = connection(&path)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = project_id.to_string();
        for table in [
            "reference_apply_operations",
            "agent_reference_bindings",
            "project_reference_configs",
            "project_review_runs",
            "project_review_jobs",
            "project_review_ci_watches",
            "project_pull_request_states",
            "project_review_cleanup_tasks",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE project_id=?1"), [&id])?;
        }
        tx.execute("DELETE FROM projects WHERE id=?1", [&id])?;
        tx.commit()?;
        Ok(())
    })
    .await
}
