//! 父行的持久化边界：创建走显式 INSERT，普通保存只做 UPDATE。
//!
//! 约束：
//! - `insert_*_row` 只负责创建，目标 ID 已存在（或 Agent 已退休）时显式失败；
//! - `save_*_row` 只做 UPDATE，目标行不存在（例如已经被删除）时显式失败，绝不通过 UPSERT
//!   复活父行，也不会重建被删除的空引用配置；
//! - 引用绑定仍然拥有 `container_id` 投影的写权限，普通保存不得改写 `created_at`；
//! - `retired_agent_sessions` 的墓碑既不能被普通保存清除，也不能被已退休 ID 的重用绕过；
//!   墓碑只由 PL 会话退休完成后的 `complete_agent_session_retirement` 清理；
//! - 项目的 `Deleting` 是删除终态，普通保存不得把它改回其它状态。
use crate::*;
use mai_protocol::ProjectStatus;
use rusqlite::{Connection, OptionalExtension, params};

const PARENT_AGENT_MISSING: &str = "parent_agent_missing";
const PARENT_AGENT_EXISTS: &str = "parent_agent_exists";
const AGENT_ID_RETIRED: &str = "agent_id_retired";
const PARENT_PROJECT_MISSING: &str = "parent_project_missing";
const PARENT_PROJECT_EXISTS: &str = "parent_project_exists";
const PARENT_PROJECT_DELETING: &str = "parent_project_deleting";

/// 父行生命周期的类型化冲突；沿用 store 现有的 `ReferenceConflict { code, message }` 形状。
fn parent_conflict(code: &'static str, message: impl Into<String>) -> StoreError {
    StoreError::ReferenceConflict {
        code,
        message: message.into(),
    }
}

pub(crate) fn connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(connection)
}

pub(crate) async fn sqlite_task<T: Send + 'static>(
    task: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| StoreError::DataIntegrity(format!("SQLite 工作线程失败: {error}")))?
}

pub(crate) async fn save_agent_row(
    store: &MaiStore,
    summary: &AgentSummary,
    system_prompt: Option<&str>,
) -> Result<()> {
    let path = store.path.clone();
    let summary = summary.clone();
    let system_prompt = system_prompt.map(str::to_owned);
    sqlite_task(move || {
        let mut connection = connection(&path)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // 只 UPDATE：行不存在就是"已经被删除"，旧摘要既不能复活父行，也不能顺手清掉墓碑。
        let sql = "UPDATE agents SET parent_id=?2, task_id=?3, project_id=?4, role=?5, profile_id=?6, workspace_json=?7, review_run_id=?8, name=?9, resource_state=?10, resource_error=?11, container_id=COALESCE((SELECT container_id FROM agent_reference_bindings WHERE agent_id=agents.id), ?12), docker_image=?13, provider_id=?14, provider_name=?15, model=?16, reasoning_effort=?17, updated_at=?18, system_prompt=?19 WHERE id=?1";
        let changed = tx.execute(sql, params![
            summary.id.to_string(),
            summary.parent_id.map(|id| id.to_string()),
            summary.task_id.map(|id| id.to_string()),
            summary.project_id.map(|id| id.to_string()),
            summary.role.map(|r| r.to_string()),
            summary.profile_id.clone(),
            summary
                .workspace
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            summary.review_run_id.map(|id| id.to_string()),
            summary.name.clone(),
            summary.resource.state.to_string(),
            summary.resource.error.clone(),
            summary.container_id.clone(),
            summary.docker_image.clone(),
            summary.provider_id.clone(),
            summary.provider_name.clone(),
            summary.model.clone(),
            summary.reasoning_effort.clone(),
            summary.updated_at.to_rfc3339(),
            system_prompt
        ])?;
        if changed != 1 {
            return Err(parent_conflict(
                PARENT_AGENT_MISSING,
                format!(
                    "Agent `{}` 已不存在，拒绝用旧摘要覆盖已删除的父行",
                    summary.id
                ),
            ));
        }
        tx.commit()?;
        Ok(())
    }).await
}

/// 创建一条新的产品 Agent 行。
///
/// 已存在同 ID 的 Agent、或该 ID 还带着退休墓碑时都显式失败：墓碑表示旧会话仍在退休流程里，
/// 只有 `complete_agent_session_retirement` 完成后才能重新使用该 ID。
pub(crate) async fn insert_agent_row(
    store: &MaiStore,
    summary: &AgentSummary,
    system_prompt: Option<&str>,
) -> Result<()> {
    let path = store.path.clone();
    let summary = summary.clone();
    let system_prompt = system_prompt.map(str::to_owned);
    sqlite_task(move || {
        let mut connection = connection(&path)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = summary.id.to_string();
        let existing: Option<String> = tx
            .query_row("SELECT id FROM agents WHERE id=?1", [&id], |row| row.get(0))
            .optional()?;
        if existing.is_some() {
            return Err(parent_conflict(
                PARENT_AGENT_EXISTS,
                format!("Agent `{id}` 已经存在，创建必须使用新的 ID"),
            ));
        }
        let retired: Option<String> = tx
            .query_row(
                "SELECT agent_id FROM retired_agent_sessions WHERE agent_id=?1",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        if retired.is_some() {
            return Err(parent_conflict(
                AGENT_ID_RETIRED,
                format!("Agent `{id}` 的会话仍在退休流程中，拒绝重用该 ID"),
            ));
        }
        tx.execute(
            "INSERT INTO agents (id, parent_id, task_id, project_id, role, profile_id, workspace_json, review_run_id, name, resource_state, resource_error, container_id, docker_image, provider_id, provider_name, model, reasoning_effort, created_at, updated_at, system_prompt) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                id,
                summary.parent_id.map(|id| id.to_string()),
                summary.task_id.map(|id| id.to_string()),
                summary.project_id.map(|id| id.to_string()),
                summary.role.map(|r| r.to_string()),
                summary.profile_id.clone(),
                summary
                    .workspace
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                summary.review_run_id.map(|id| id.to_string()),
                summary.name.clone(),
                summary.resource.state.to_string(),
                summary.resource.error.clone(),
                summary.container_id.clone(),
                summary.docker_image.clone(),
                summary.provider_id.clone(),
                summary.provider_name.clone(),
                summary.model.clone(),
                summary.reasoning_effort.clone(),
                summary.created_at.to_rfc3339(),
                summary.updated_at.to_rfc3339(),
                system_prompt
            ],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub(crate) async fn save_project_row(store: &MaiStore, project: &ProjectSummary) -> Result<()> {
    let path = store.path.clone();
    let project = project.clone();

    sqlite_task(move || {
        let mut connection = connection(&path)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = project.id.to_string();
        // 只 UPDATE：行不存在就是"已经被删除"，旧摘要不能复活项目，也不能重建空引用配置。
        let current: Option<String> = tx
            .query_row("SELECT status FROM projects WHERE id=?1", [&id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(current) = current else {
            return Err(parent_conflict(
                PARENT_PROJECT_MISSING,
                format!("项目 `{id}` 已不存在，拒绝用旧摘要覆盖已删除的父行"),
            ));
        };
        let status = project.status.to_string();
        if current == ProjectStatus::Deleting.to_string()
            && status != ProjectStatus::Deleting.to_string()
        {
            return Err(parent_conflict(
                PARENT_PROJECT_DELETING,
                format!("项目 `{id}` 正在删除，拒绝把状态改回 `{status}`"),
            ));
        }
        let sql = "UPDATE projects SET name=?2, status=?3, owner=?4, repo=?5, repository_full_name=?6, git_account_id=?7, repository_id=?8, installation_id=?9, installation_account=?10, branch=?11, docker_image=?12, clone_status=?13, maintainer_agent_id=?14, updated_at=?15, last_error=?16, auto_review_enabled=?17, reviewer_extra_prompt=?18, review_status=?19, current_reviewer_agent_id=?20, last_review_started_at=?21, last_review_finished_at=?22, next_review_at=?23, last_review_outcome=?24, review_last_error=?25 WHERE id=?1";
        let changed = tx.execute(sql, params![
            id.clone(),
            project.name.clone(),
            status,
            project.owner.clone(),
            project.repo.clone(),
            project.repository_full_name.clone(),
            project.git_account_id.clone(),
            u64_to_i64(project.repository_id),
            u64_to_i64(project.installation_id),
            project.installation_account.clone(),
            project.branch.clone(),
            project.docker_image.clone(),
            project.clone_status.to_string(),
            project.maintainer_agent_id.to_string(),
            project.updated_at.to_rfc3339(),
            project.last_error.clone(),
            project.auto_review_enabled,
            project.reviewer_extra_prompt.clone(),
            project.review_status.to_string(),
            project.current_reviewer_agent_id.map(|id| id.to_string()),
            project.last_review_started_at.map(|time| time.to_rfc3339()),
            project
                .last_review_finished_at
                .map(|time| time.to_rfc3339()),
            project.next_review_at.map(|time| time.to_rfc3339()),
            project.last_review_outcome.as_ref().map(|o| o.to_string()),
            project.review_last_error.clone()
        ])?;
        if changed != 1 {
            return Err(parent_conflict(
                PARENT_PROJECT_MISSING,
                format!("项目 `{id}` 已不存在，拒绝用旧摘要覆盖已删除的父行"),
            ));
        }
        tx.commit()?;
        Ok(())
    }).await
}

/// 创建一条新的产品项目行，并同时初始化它的引用配置行。
///
/// 已存在同 ID 的项目时显式失败：创建 API 不是 UPSERT，调用方必须先用 `save_project` 更新。
pub(crate) async fn insert_project_row(store: &MaiStore, project: &ProjectSummary) -> Result<()> {
    let path = store.path.clone();
    let project = project.clone();

    sqlite_task(move || {
        let mut connection = connection(&path)?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = project.id.to_string();
        let existing: Option<String> = tx
            .query_row("SELECT id FROM projects WHERE id=?1", [&id], |row| {
                row.get(0)
            })
            .optional()?;
        if existing.is_some() {
            return Err(parent_conflict(
                PARENT_PROJECT_EXISTS,
                format!("项目 `{id}` 已经存在，创建必须使用新的 ID"),
            ));
        }
        tx.execute(
            "INSERT INTO projects (id, name, status, owner, repo, repository_full_name, git_account_id, repository_id, installation_id, installation_account, branch, docker_image, clone_status, maintainer_agent_id, created_at, updated_at, last_error, auto_review_enabled, reviewer_extra_prompt, review_status, current_reviewer_agent_id, last_review_started_at, last_review_finished_at, next_review_at, last_review_outcome, review_last_error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
            params![
                id,
                project.name.clone(),
                project.status.to_string(),
                project.owner.clone(),
                project.repo.clone(),
                project.repository_full_name.clone(),
                project.git_account_id.clone(),
                u64_to_i64(project.repository_id),
                u64_to_i64(project.installation_id),
                project.installation_account.clone(),
                project.branch.clone(),
                project.docker_image.clone(),
                project.clone_status.to_string(),
                project.maintainer_agent_id.to_string(),
                project.created_at.to_rfc3339(),
                project.updated_at.to_rfc3339(),
                project.last_error.clone(),
                project.auto_review_enabled,
                project.reviewer_extra_prompt.clone(),
                project.review_status.to_string(),
                project.current_reviewer_agent_id.map(|id| id.to_string()),
                project.last_review_started_at.map(|time| time.to_rfc3339()),
                project
                    .last_review_finished_at
                    .map(|time| time.to_rfc3339()),
                project.next_review_at.map(|time| time.to_rfc3339()),
                project.last_review_outcome.as_ref().map(|o| o.to_string()),
                project.review_last_error.clone()
            ],
        )?;
        tx.execute(
            "INSERT INTO project_reference_configs(project_id,revision,bindings_json,updated_at) VALUES(?1,0,'[]',?2) ON CONFLICT(project_id) DO NOTHING",
            params![project.id.to_string(), project.updated_at.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}
