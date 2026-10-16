//! 环境发布事务的删除边界及同修订物理重核验。

use super::*;
use crate::{PersistedReferenceBinding, ReferenceApplyScope};
use mai_protocol::{AppliedReferenceSnapshot, ReferenceApplyPolicy};
use pretty_assertions::assert_eq;

async fn bound_environment() -> (TempDir, MaiStore, ProjectSummary, PersistedReferenceBinding) {
    let (directory, store) = store().await;
    let mut agent = test_agent_summary(Uuid::new_v4(), None, Utc::now());
    let project = test_project_summary(Uuid::new_v4(), agent.id);
    agent.project_id = Some(project.id);
    store.insert_project(&project).await.expect("创建项目");
    store.insert_agent(&agent, None).await.expect("创建 Agent");
    let snapshot = AppliedReferenceSnapshot {
        project_id: project.id,
        configuration_revision: 0,
        fingerprint: "empty-reference-environment".into(),
        references: Vec::new(),
        observed_at: Utc::now(),
    };
    let binding = PersistedReferenceBinding {
        agent_id: agent.id,
        project_id: Some(project.id),
        generation: Uuid::new_v4(),
        container_id: "verified-container".into(),
        effective_image_id: "sha256:verified".into(),
        applied_revision: 0,
        fingerprint: snapshot.fingerprint.clone(),
        snapshot,
        updated_at: Utc::now(),
    };
    store
        .publish_reference_binding(binding.clone())
        .await
        .expect("发布初始环境");
    (directory, store, project, binding)
}

#[tokio::test]
async fn deleting_project_cannot_publish_or_enqueue_environment() {
    let (_directory, store, mut project, binding) = bound_environment().await;
    project.status = ProjectStatus::Deleting;
    store.save_project(&project).await.expect("开始删除");
    let mut stale = binding.clone();
    stale.container_id = "late-candidate".into();
    let publish = store
        .publish_reference_binding(stale)
        .await
        .expect_err("删除期间拒绝发布");
    assert!(matches!(
        publish,
        StoreError::ReferenceConflict {
            code: "project_deleting",
            ..
        }
    ));
    let enqueue = store
        .enqueue_reference_application(
            project.id,
            binding.snapshot.clone(),
            ReferenceApplyPolicy::WhenIdle,
            ReferenceApplyScope::AllPhysicalEnvironments,
        )
        .await
        .expect_err("删除期间拒绝新应用");
    assert!(matches!(
        enqueue,
        StoreError::ReferenceConflict {
            code: "project_deleting",
            ..
        }
    ));
    assert_eq!(
        json!(binding),
        json!(
            store
                .reference_binding(binding.agent_id)
                .await
                .expect("读取绑定")
                .expect("保留原绑定")
        )
    );
    assert_eq!(
        0,
        store.reference_operations().await.expect("读取收据").len()
    );
}

#[tokio::test]
async fn explicit_apply_rechecks_physical_environment_at_the_same_revision() {
    let (_directory, store, project, binding) = bound_environment().await;
    let ordinary = store
        .enqueue_reference_application(
            project.id,
            binding.snapshot.clone(),
            ReferenceApplyPolicy::WhenIdle,
            ReferenceApplyScope::Unapplied,
        )
        .await
        .expect("普通补齐");
    assert_eq!(Vec::<Uuid>::new(), ordinary);
    let explicit = store
        .enqueue_reference_application(
            project.id,
            binding.snapshot.clone(),
            ReferenceApplyPolicy::WhenIdle,
            ReferenceApplyScope::AllPhysicalEnvironments,
        )
        .await
        .expect("显式应用重核验");
    assert_eq!(1, explicit.len());
    let operations = store.reference_operations().await.expect("读取收据");
    assert_eq!(
        vec![(binding.agent_id, 0)],
        operations
            .into_iter()
            .map(|receipt| (
                receipt.operation.agent_id,
                receipt.operation.target_revision
            ))
            .collect::<Vec<_>>()
    );
}
