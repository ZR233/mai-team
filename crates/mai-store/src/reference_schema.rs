//! 新库与离线迁移共用的唯一引用 schema 定义。
use crate::*;
use rusqlite::Connection;

pub(crate) const REFERENCE_SCHEMA: &str = r#"
CREATE TABLE reference_sources (
 id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, revision_label TEXT NOT NULL,
 provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json)),
 canonical_path TEXT UNIQUE NOT NULL, manifest_digest TEXT NOT NULL,
 source_dev TEXT NOT NULL, source_ino TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('validating','ready','invalid','disabled','missing')),
 error TEXT, file_count TEXT NOT NULL, byte_count TEXT NOT NULL,
 approved_for_agent_access INTEGER NOT NULL CHECK(approved_for_agent_access IN (0,1)),
 warnings_json TEXT NOT NULL CHECK(json_valid(warnings_json)), created_at TEXT NOT NULL, updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE project_reference_configs (
 project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
 revision INTEGER NOT NULL CHECK(revision BETWEEN 0 AND 9007199254740991),
 bindings_json TEXT NOT NULL CHECK(json_valid(bindings_json) AND json_type(bindings_json)='array'),
 updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE agent_reference_bindings (
 agent_id TEXT PRIMARY KEY NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
 project_id TEXT REFERENCES projects(id) ON DELETE RESTRICT,
 generation TEXT NOT NULL, container_id TEXT NOT NULL UNIQUE,
 effective_image_id TEXT NOT NULL, applied_revision INTEGER NOT NULL,
 fingerprint TEXT NOT NULL, snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
 updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE reference_apply_operations (
 id TEXT PRIMARY KEY NOT NULL, agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
 project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
 target_revision INTEGER NOT NULL, target_snapshot_json TEXT NOT NULL CHECK(json_valid(target_snapshot_json)),
 policy TEXT NOT NULL CHECK(policy IN ('when_idle','stop_and_apply')),
 phase TEXT NOT NULL CHECK(phase IN ('waiting_idle','draining','snapshotting','creating','verifying','rebinding','publishing','cleanup_pending','succeeded','failed','superseded')),
 old_container_id TEXT, candidate_container_id TEXT, snapshot_image_id TEXT,
 old_mounts_json TEXT CHECK(old_mounts_json IS NULL OR json_valid(old_mounts_json)),
 error_json TEXT CHECK(error_json IS NULL OR json_valid(error_json)),
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX reference_apply_one_active_agent ON reference_apply_operations(agent_id)
 WHERE phase NOT IN ('succeeded','failed','superseded');
CREATE INDEX reference_apply_project ON reference_apply_operations(project_id, target_revision);
INSERT INTO project_reference_configs(project_id,revision,bindings_json,updated_at)
 SELECT id,0,'[]',updated_at FROM projects;
"#;

pub(crate) fn create_reference_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(REFERENCE_SCHEMA)?;
    Ok(())
}
