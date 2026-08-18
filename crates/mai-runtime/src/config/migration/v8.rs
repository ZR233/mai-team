use mai_store::ConfigDocumentStore;
use pl_core::AgentModelConfig;
use pl_model::WebSearchConfig;
use serde::{Deserialize, Serialize};

use super::{backup_document, canonicalize_preset_providers, migrate_v4_provider_capabilities};
use crate::Result;
use crate::config::{
    MAI_CONFIG_SCHEMA_VERSION, MaiConfig, MaiContainerConfig, MaiGithubConfig,
    MaiInstructionsConfig, MaiMcpConfig, MaiRetentionConfig, MaiReviewConfig, MaiSkillsConfig,
};

/// schema 4、5、7、8 共享的产品配置形态。
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct LegacyCurrentMaiConfig {
    pub(super) schema_version: u32,
    pub(super) models: AgentModelConfig,
    #[serde(default)]
    web_search: WebSearchConfig,
    #[serde(default)]
    containers: MaiContainerConfig,
    #[serde(default)]
    instructions: MaiInstructionsConfig,
    #[serde(default)]
    skills: MaiSkillsConfig,
    #[serde(default)]
    mcp: MaiMcpConfig,
    #[serde(default)]
    github: MaiGithubConfig,
    #[serde(default)]
    review: MaiReviewConfig,
    #[serde(default)]
    pub(super) retention: LegacyRetentionConfig,
}

/// schema 8 及更早配置使用的双 Review 保留期。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyRetentionConfig {
    pub(super) review_jobs_days: i64,
    pub(super) review_runs_days: i64,
    product_events_days: i64,
    agent_logs_days: i64,
    tool_traces_days: i64,
    tool_output_days: i64,
    cleanup_interval_secs: u64,
    cleanup_batch_size: usize,
}

impl Default for LegacyRetentionConfig {
    fn default() -> Self {
        Self {
            review_jobs_days: 30,
            review_runs_days: 7,
            product_events_days: 7,
            agent_logs_days: 7,
            tool_traces_days: 7,
            tool_output_days: 14,
            cleanup_interval_secs: 3600,
            cleanup_batch_size: 500,
        }
    }
}

impl LegacyRetentionConfig {
    pub(super) fn into_current(self) -> MaiRetentionConfig {
        MaiRetentionConfig {
            review_history_days: self.review_jobs_days.min(self.review_runs_days),
            product_events_days: self.product_events_days,
            agent_logs_days: self.agent_logs_days,
            tool_traces_days: self.tool_traces_days,
            tool_output_days: self.tool_output_days,
            cleanup_interval_secs: self.cleanup_interval_secs,
            cleanup_batch_size: self.cleanup_batch_size,
        }
    }
}

pub(super) async fn migrate(
    documents: &ConfigDocumentStore,
    legacy: LegacyCurrentMaiConfig,
) -> Result<MaiConfig> {
    let previous_version = legacy.schema_version;
    let mut config = MaiConfig {
        schema_version: MAI_CONFIG_SCHEMA_VERSION,
        models: legacy.models,
        web_search: legacy.web_search,
        containers: legacy.containers,
        instructions: legacy.instructions,
        skills: legacy.skills,
        mcp: legacy.mcp,
        github: legacy.github,
        review: legacy.review,
        retention: legacy.retention.into_current(),
    };
    if previous_version == 4 {
        migrate_v4_provider_capabilities(&mut config.models);
    }
    canonicalize_preset_providers(&mut config.models)?;
    config.validate()?;
    backup_document(documents.path(), previous_version).await?;
    documents.save(&config).await?;
    Ok(config)
}

#[cfg(test)]
pub(super) fn legacy_current_config(schema_version: u32) -> LegacyCurrentMaiConfig {
    let current = MaiConfig::default();
    LegacyCurrentMaiConfig {
        schema_version,
        models: current.models,
        web_search: current.web_search,
        containers: current.containers,
        instructions: current.instructions,
        skills: current.skills,
        mcp: current.mcp,
        github: current.github,
        review: current.review,
        retention: LegacyRetentionConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_migration_uses_shorter_legacy_window() {
        let retention = LegacyRetentionConfig {
            review_jobs_days: 5,
            review_runs_days: 30,
            ..LegacyRetentionConfig::default()
        };

        assert_eq!(retention.into_current().review_history_days, 5);
    }

    #[tokio::test]
    async fn schema_eight_unifies_review_retention_without_old_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let documents = ConfigDocumentStore::new(path.clone());
        let mut config = legacy_current_config(8);
        config.retention.review_jobs_days = 30;
        config.retention.review_runs_days = 7;
        documents.save(&config).await.unwrap();

        let migrated = super::super::migrate(&documents).await.unwrap();
        let persisted = tokio::fs::read_to_string(&path).await.unwrap();

        assert_eq!(migrated.retention.review_history_days, 7);
        assert!(persisted.contains("review_history_days = 7"));
        assert!(!persisted.contains("review_jobs_days"));
        assert!(!persisted.contains("review_runs_days"));
        assert!(
            tokio::fs::try_exists(super::super::backup_path(&path, 8))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn schema_eight_rejects_mixed_retention_shapes_without_rewriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let documents = ConfigDocumentStore::new(path.clone());
        let mut value = toml::Value::try_from(legacy_current_config(8)).unwrap();
        value
            .get_mut("retention")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert("review_history_days".to_string(), toml::Value::Integer(7));
        tokio::fs::write(&path, toml::to_string_pretty(&value).unwrap())
            .await
            .unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        assert!(super::super::migrate(&documents).await.is_err());
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert!(
            !tokio::fs::try_exists(super::super::backup_path(&path, 8))
                .await
                .unwrap()
        );
    }
}
