//! 镜像回收只读取持久化所有权；失败收据仍然拥有恢复资源。
use super::*;
use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct ReferenceResourceOwners {
    pub image_ids: HashSet<String>,
    pub operation_ids: HashSet<Uuid>,
    pub agent_ids: HashSet<Uuid>,
}

impl MaiStore {
    /// 在同一读事务中取得当前绑定、非终态操作和仍存活 Agent 的镜像所有权。
    pub async fn reference_resource_owners(&self) -> Result<ReferenceResourceOwners> {
        let path = self.path.clone();
        sqlite_task(move || {
            let mut connection = connection(&path)?;
            let transaction = connection.transaction()?;
            let mut owners = ReferenceResourceOwners::default();
            {
                let mut query = transaction.prepare("SELECT DISTINCT effective_image_id FROM agent_reference_bindings")?;
                for row in query.query_map([], |row| row.get::<_, String>(0))? {
                    owners.image_ids.insert(row?);
                }
            }
            for (sql, destination) in [
                ("SELECT id FROM reference_apply_operations WHERE phase NOT IN ('succeeded','superseded')", &mut owners.operation_ids),
                ("SELECT id FROM agents", &mut owners.agent_ids),
            ] {
                let mut query = transaction.prepare(sql)?;
                for row in query.query_map([], |row| row.get::<_, String>(0))? {
                    let id = row?;
                    destination.insert(Uuid::parse_str(&id).map_err(|error| StoreError::DataIntegrity(error.to_string()))?);
                }
            }
            transaction.commit()?;
            Ok(owners)
        }).await
    }
}
