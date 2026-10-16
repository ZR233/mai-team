//! 引用配置、实际绑定与换代收据的事务边界。
mod configurations;
mod operations;
mod resources;
pub use resources::ReferenceResourceOwners;
mod sources;

use crate::parent_saves::{connection, sqlite_task};
use crate::*;
use mai_protocol::*;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

pub use operations::{PersistedReferenceBinding, PersistedReferenceOperation, ReferenceApplyScope};

fn conflict(code: &'static str, message: impl Into<String>) -> StoreError {
    StoreError::ReferenceConflict {
        code,
        message: message.into(),
    }
}

fn source_is_used(connection: &Connection, source_id: Uuid) -> Result<bool> {
    let mut configs = connection.prepare("SELECT bindings_json FROM project_reference_configs")?;
    for json in configs.query_map([], |row| row.get::<_, String>(0))? {
        let bindings: Vec<ProjectReferenceBinding> = serde_json::from_str(&json?)?;
        if bindings
            .iter()
            .any(|binding| binding.source_id == source_id)
        {
            return Ok(true);
        }
    }
    for query in [
        "SELECT snapshot_json FROM agent_reference_bindings",
        "SELECT target_snapshot_json FROM reference_apply_operations WHERE phase NOT IN ('succeeded','superseded')",
    ] {
        let mut statement = connection.prepare(query)?;
        for json in statement.query_map([], |row| row.get::<_, String>(0))? {
            let snapshot: AppliedReferenceSnapshot = serde_json::from_str(&json?)?;
            if snapshot
                .references
                .iter()
                .any(|reference| reference.source_id == source_id)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
