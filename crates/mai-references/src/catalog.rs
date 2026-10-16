//! 登记与异步验证由领域服务拥有；数据库是状态真相，通知仅用于刷新。
use crate::*;
use chrono::Utc;
use mai_protocol::*;
use mai_store::MaiStore;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{RwLock, Semaphore, broadcast};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum ReferenceCatalogChange {
    Source { source_id: Uuid, state: SourceState },
}

pub struct ReferenceCatalog {
    pub(crate) store: Arc<MaiStore>,
    pub(crate) policy: Option<ReferenceDeploymentPolicy>,
    validation: Arc<Semaphore>,
    verified: RwLock<HashMap<Uuid, ValidatedReferenceSource>>,
    changes: broadcast::Sender<ReferenceCatalogChange>,
}

impl ReferenceCatalog {
    pub fn new(store: Arc<MaiStore>, policy: Option<ReferenceDeploymentPolicy>) -> Arc<Self> {
        Arc::new(Self {
            store,
            policy,
            validation: Arc::new(Semaphore::new(1)),
            verified: RwLock::new(HashMap::new()),
            changes: broadcast::channel(128).0,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ReferenceCatalogChange> {
        self.changes.subscribe()
    }

    pub fn deployment_policy(&self) -> Result<&ReferenceDeploymentPolicy> {
        self.policy.as_ref().ok_or_else(|| {
            ReferenceError::new(
                ReferenceErrorCode::RecursiveReadOnlyUnsupported,
                "管理员尚未配置参考源码允许根",
            )
        })
    }

    pub async fn sources(&self) -> Result<Vec<ReferenceSource>> {
        self.store.reference_sources().await.map_err(storage_error)
    }

    pub async fn source(&self, id: Uuid) -> Result<ReferenceSource> {
        self.sources()
            .await?
            .into_iter()
            .find(|source| source.id == id)
            .ok_or_else(|| ReferenceError::new(ReferenceErrorCode::SourceNotFound, "参考源不存在"))
    }

    pub async fn register(
        self: &Arc<Self>,
        mut request: NewReferenceSourceRequest,
    ) -> Result<ReferenceSource> {
        let policy = self.deployment_policy()?;
        let path = validate_source_root(policy, &request.host_path)?;
        request.host_path = path.path().to_string_lossy().into_owned();
        let source = validating_source_record(Uuid::new_v4(), &request, Utc::now())?;
        self.store
            .insert_reference_source(source.clone())
            .await
            .map_err(storage_error)?;
        self.schedule_validation(source.clone());
        Ok(source)
    }

    pub async fn verify(self: &Arc<Self>, id: Uuid) -> Result<ReferenceSource> {
        self.deployment_policy()?;
        let mut source = self.source(id).await?;
        self.store
            .begin_reference_validation(id)
            .await
            .map_err(storage_error)?;
        source.state = SourceState::Validating;
        self.schedule_validation(source.clone());
        Ok(source)
    }

    pub async fn patch(
        self: &Arc<Self>,
        id: Uuid,
        patch: PatchReferenceSourceRequest,
    ) -> Result<ReferenceSource> {
        let current = self.source(id).await?;
        validating_source_record(
            id,
            &NewReferenceSourceRequest {
                name: patch.name.clone().unwrap_or(current.name),
                host_path: current.host_path,
                revision_label: patch
                    .revision_label
                    .clone()
                    .unwrap_or(current.revision_label),
                approved_for_agent_access: patch
                    .approved_for_agent_access
                    .unwrap_or(current.approved_for_agent_access),
            },
            current.created_at,
        )?;
        let source = self
            .store
            .patch_reference_source(id, patch)
            .await
            .map_err(storage_error)?;
        if source.state == SourceState::Validating {
            self.schedule_validation(source.clone());
        }
        let _ = self.changes.send(ReferenceCatalogChange::Source {
            source_id: id,
            state: source.state,
        });
        Ok(source)
    }

    pub async fn unregister(&self, id: Uuid) -> Result<()> {
        self.store
            .delete_reference_source(id)
            .await
            .map_err(storage_error)?;
        self.verified.write().await.remove(&id);
        let _ = self.changes.send(ReferenceCatalogChange::Source {
            source_id: id,
            state: SourceState::Disabled,
        });
        Ok(())
    }

    pub async fn resume_validations(self: &Arc<Self>) -> Result<()> {
        for source in self.sources().await? {
            if source.state == SourceState::Validating {
                self.schedule_validation(source);
            }
        }
        Ok(())
    }

    fn schedule_validation(self: &Arc<Self>, source: ReferenceSource) {
        let catalog = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = catalog.finish_validation(source).await {
                tracing::error!(%error,"参考源验证状态保存失败，将由下次启动继续");
            }
        });
    }

    async fn finish_validation(&self, mut source: ReferenceSource) -> Result<()> {
        let policy = self.deployment_policy()?.clone();
        let request = NewReferenceSourceRequest {
            name: source.name.clone(),
            host_path: source.host_path.clone(),
            revision_label: source.revision_label.clone(),
            approved_for_agent_access: source.approved_for_agent_access,
        };
        let original = source.clone();
        let validation_request = request.clone();
        let _permit = self.validation.acquire().await.map_err(|error| {
            ReferenceError::new(ReferenceErrorCode::InternalError, error.to_string())
        })?;
        let outcome = tokio::task::spawn_blocking(move || {
            if original.manifest_digest.is_some() {
                revalidate_source(&policy, &original, Utc::now())
            } else {
                validate_new_source(&policy, &validation_request, Utc::now())
            }
        })
        .await
        .map_err(|error| {
            ReferenceError::new(ReferenceErrorCode::InternalError, error.to_string())
        })?;
        match outcome {
            Ok(validated) => {
                source = ready_source_record(
                    source.id,
                    &request,
                    &validated,
                    source.created_at,
                    Utc::now(),
                );
                self.verified.write().await.insert(source.id, validated);
            }
            Err(error) => {
                source.state = if error.code() == ReferenceErrorCode::SourceNotFound {
                    SourceState::Missing
                } else {
                    SourceState::Invalid
                };
                source.validation_error = Some(ReferenceValidationFailure {
                    code: error.code(),
                    message: error.message().to_owned(),
                    path: None,
                    observed_at: Utc::now(),
                });
                source.updated_at = Utc::now();
                self.verified.write().await.remove(&source.id);
            }
        }
        if self
            .store
            .complete_reference_validation(source.clone())
            .await
            .map_err(storage_error)?
        {
            let _ = self.changes.send(ReferenceCatalogChange::Source {
                source_id: source.id,
                state: source.state,
            });
        }
        Ok(())
    }

    pub async fn snapshot(
        &self,
        project_id: ProjectId,
        configuration: &ProjectReferenceConfiguration,
    ) -> Result<AppliedReferenceSnapshot> {
        if configuration.bindings.is_empty() {
            return build_snapshot(project_id, configuration.revision, Vec::new(), Utc::now());
        }
        let policy = self.deployment_policy()?;
        validate_bindings(policy, &configuration.bindings)?;
        let sources = self.sources().await?;
        let mut resolved = Vec::new();
        for binding in configuration
            .bindings
            .iter()
            .filter(|binding| binding.enabled)
        {
            let source = sources
                .iter()
                .find(|source| source.id == binding.source_id)
                .ok_or_else(|| {
                    ReferenceError::new(ReferenceErrorCode::SourceNotFound, "启用的参考源不存在")
                })?;
            let root = validate_source_root(policy, &source.host_path)?;
            if root.device() != source.source_device || root.inode() != source.source_inode {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::SourceIdentityChanged,
                    "参考源目录身份发生变化",
                ));
            }
            let cached = self.verified.read().await.get(&source.id).cloned();
            let validated = match cached {
                Some(validated) => validated,
                None => {
                    let _permit = self.validation.acquire().await.map_err(|error| {
                        ReferenceError::new(ReferenceErrorCode::InternalError, error.to_string())
                    })?;
                    // 等待同一个验证额度后再读缓存，多个 Agent 不重复扫描同一份 Linux 树。
                    let ready = self.verified.read().await.get(&source.id).cloned();
                    match ready {
                        Some(validated) => validated,
                        None => {
                            let source = source.clone();
                            let policy = policy.clone();
                            let validated = tokio::task::spawn_blocking(move || {
                                revalidate_source(&policy, &source, Utc::now())
                            })
                            .await
                            .map_err(|error| {
                                ReferenceError::new(
                                    ReferenceErrorCode::InternalError,
                                    error.to_string(),
                                )
                            })??;
                            self.verified
                                .write()
                                .await
                                .insert(binding.source_id, validated.clone());
                            validated
                        }
                    }
                }
            };
            resolved.push(ResolvedReference::resolve(binding, source, &validated)?);
        }
        build_snapshot(project_id, configuration.revision, resolved, Utc::now())
    }
}

pub(crate) fn storage_error(error: mai_store::StoreError) -> ReferenceError {
    let code = match &error {
        mai_store::StoreError::ReferenceConflict {
            code: "revision_conflict",
            ..
        } => ReferenceErrorCode::ReferenceRevisionConflict,
        mai_store::StoreError::ReferenceConflict {
            code: "source_in_use",
            ..
        } => ReferenceErrorCode::SourceInUse,
        mai_store::StoreError::ReferenceConflict {
            code: "source_not_found",
            ..
        } => ReferenceErrorCode::SourceNotFound,
        mai_store::StoreError::ReferenceConflict {
            code: "source_identity_changed",
            ..
        } => ReferenceErrorCode::SourceIdentityChanged,
        mai_store::StoreError::ReferenceConflict {
            code: "project_deleting",
            ..
        } => ReferenceErrorCode::EnvironmentBusy,
        mai_store::StoreError::ReferenceConflict {
            code: "agent_deleted",
            ..
        } => ReferenceErrorCode::EnvironmentSuperseded,
        mai_store::StoreError::ReferenceConflict { .. } => {
            ReferenceErrorCode::EnvironmentApplyFailed
        }
        mai_store::StoreError::Toasty(_)
        | mai_store::StoreError::Io(_)
        | mai_store::StoreError::Json(_)
        | mai_store::StoreError::Sqlite(_)
        | mai_store::StoreError::Toml(_)
        | mai_store::StoreError::TomlSer(_)
        | mai_store::StoreError::Time(_)
        | mai_store::StoreError::Parse(_)
        | mai_store::StoreError::DataIntegrity(_)
        | mai_store::StoreError::InvalidConfig(_) => {
            ReferenceErrorCode::ReferenceStorageUnavailable
        }
    };
    ReferenceError::new(code, error.to_string()).with_cause(error)
}

#[cfg(test)]
#[path = "catalog_recovery_tests.rs"]
mod tests;
