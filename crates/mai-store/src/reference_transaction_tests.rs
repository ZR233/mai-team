//! 引用 CAS 的并发、事务写失败和丢失响应后的 durable 重读。
use super::*;
use mai_protocol::{
    AppliedReferenceSnapshot, ProjectReferenceBinding, ReplaceProjectReferencesRequest,
};
use pretty_assertions::assert_eq;

async fn fixture() -> (TempDir, Arc<MaiStore>, ProjectSummary, AgentSummary) {
    let (directory, store) = store().await;
    let mut agent = test_agent_summary(Uuid::new_v4(), None, Utc::now());
    let project = test_project_summary(Uuid::new_v4(), agent.id);
    agent.project_id = Some(project.id);
    agent.container_id = Some("preserved-container".into());
    store.insert_project(&project).await.unwrap();
    store.insert_agent(&agent, None).await.unwrap();
    (directory, Arc::new(store), project, agent)
}

fn change(
    project: ProjectId,
    alias: &str,
) -> (ReplaceProjectReferencesRequest, AppliedReferenceSnapshot) {
    (
        ReplaceProjectReferencesRequest {
            expected_revision: 0,
            bindings: vec![ProjectReferenceBinding {
                id: Uuid::new_v4(),
                source_id: Uuid::new_v4(),
                alias: alias.into(),
                description: "停用的事务测试条目".into(),
                enabled: false,
            }],
        },
        AppliedReferenceSnapshot {
            project_id: project,
            configuration_revision: 1,
            fingerprint: "empty".into(),
            references: vec![],
            observed_at: Utc::now(),
        },
    )
}

#[tokio::test]
async fn simultaneous_reference_cas_has_exactly_one_winner_and_one_durable_intent() {
    let (_directory, store, project, _agent) = fixture().await;
    let (first, first_snapshot) = change(project.id, "first");
    let (second, second_snapshot) = change(project.id, "second");
    let (left, right) = tokio::join!(
        store.replace_project_reference_configuration(project.id, first, first_snapshot),
        store.replace_project_reference_configuration(project.id, second, second_snapshot),
    );
    let winner = match (left, right) {
        (
            Ok(winner),
            Err(StoreError::ReferenceConflict {
                code: "revision_conflict",
                ..
            }),
        )
        | (
            Err(StoreError::ReferenceConflict {
                code: "revision_conflict",
                ..
            }),
            Ok(winner),
        ) => winner,
        outcome => panic!("必须有且只有一次成功 CAS：{outcome:?}"),
    };
    assert_eq!(
        store
            .project_reference_configuration(project.id)
            .await
            .unwrap(),
        winner
    );
    let operations = store.reference_operations().await.unwrap();
    assert_eq!(operations.len(), 1);
    assert_eq!(
        operations[0].target_snapshot.configuration_revision,
        winner.revision
    );
}

#[tokio::test]
async fn intent_write_failure_rolls_back_configuration_and_container_projection() {
    let (_directory, store, project, agent) = fixture().await;
    let before = store
        .project_reference_configuration(project.id)
        .await
        .unwrap();
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_reference_intent BEFORE INSERT ON reference_apply_operations BEGIN SELECT RAISE(ABORT,'injected intent write failure'); END;").unwrap();
    let (request, snapshot) = change(project.id, "failed");
    assert!(
        store
            .replace_project_reference_configuration(project.id, request, snapshot)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .project_reference_configuration(project.id)
            .await
            .unwrap(),
        before
    );
    assert!(store.reference_operations().await.unwrap().is_empty());
    let restored = store.load_runtime_snapshot(0).await.unwrap();
    assert_eq!(
        serde_json::to_value(&persisted_agent(&restored, agent.id).summary).unwrap(),
        json!(agent)
    );
}

#[tokio::test]
async fn lost_response_and_failed_event_leave_committed_configuration_recoverable() {
    let (_directory, store, project, agent) = fixture().await;
    let (request, snapshot) = change(project.id, "saved");
    // 模拟调用端未收到成功响应：丢弃返回值，事件写入另行失败。
    store
        .replace_project_reference_configuration(project.id, request.clone(), snapshot.clone())
        .await
        .unwrap();
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_reference_event BEFORE INSERT ON product_events BEGIN SELECT RAISE(ABORT,'injected event write failure'); END;").unwrap();
    assert!(
        store
            .append_product_event(&test_product_event(1, agent.id, Utc::now()))
            .await
            .is_err()
    );
    assert!(matches!(
        store
            .replace_project_reference_configuration(project.id, request.clone(), snapshot)
            .await,
        Err(StoreError::ReferenceConflict {
            code: "revision_conflict",
            ..
        })
    ));
    let reopened = MaiStore::open_with_config_and_artifact_index_path(
        store.path(),
        _directory.path().join("config.toml"),
        _directory.path().join("artifacts/index"),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened
            .project_reference_configuration(project.id)
            .await
            .unwrap(),
        mai_protocol::ProjectReferenceConfiguration {
            revision: 1,
            bindings: request.bindings
        }
    );
    assert_eq!(reopened.reference_operations().await.unwrap().len(), 1);
}
