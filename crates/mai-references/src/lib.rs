//! 项目参考源码领域层：部署策略、源验证、规范化配置、挂载计划与固定环境租约。
//!
//! 边界：
//! - 领域、持久化和 Docker 应用状态机在此聚合；不依赖 mai-runtime，产品资源由回调装配。
//! - 这里的校验是唯一真相：UI、HTTP handler 与运行时都不能各自再写一套放宽版本。
//! - 路径、归属、文件类型、符号链接与 manifest 的检查全部 fail closed：遇到不确定
//!   的情况返回类型化错误，不静默跳过、不自动需要创建目录。

mod admission;
mod application;
mod application_cleanup;
mod resource_cleanup;
pub use resource_cleanup::reference_snapshot_owner;
mod application_host;
mod applications;
mod deployment;
mod environment;
mod observer;
pub use observer::ReferenceApplicationObserver;
mod canonical;
pub use admission::{AgentAdmission, AgentRetirement, ReferenceAdmissions};
pub use application_host::ReferenceEnvironmentHost;
pub use applications::ReferenceApplications;
pub use deployment::load_reference_deployment_policy;
pub use environment::{plan_from_environment, reference_mounts};
mod catalog;
pub use catalog::{ReferenceCatalog, ReferenceCatalogChange};
mod config;
mod error;
mod lease;
mod manifest;
mod plan;
mod policy;
mod source;
mod tree;
mod tree_mounts;

pub use config::{
    NormalizedBindings, check_expected_revision, container_reference_path, is_js_safe_revision,
    next_revision, normalize_bindings, normalize_replace_request, validate_alias,
    validate_bindings, validate_bound_sources, validate_description,
};
pub use error::{ReferenceError, Result};
pub use lease::{
    AgentEnvironmentLease, GenerationLifecycle, LeasePublication, ReferenceLeaseRegistry,
    TrackedTask,
};
pub use manifest::{
    DIRECTORY_TREE_DIGEST_FORMAT, GIT_TREE_DIGEST_FORMAT, GIT_TREE_PROVENANCE_KIND,
    SOURCE_MANIFEST_FILE_NAME, SOURCE_MANIFEST_FORMAT, SourceManifest, SourceManifestEntry,
    SourceManifestProvenance, VerifiedManifest, directory_tree_digest, load_manifest,
    locate_manifest, read_source_manifest, tree_digest_from_entries, verify_manifest,
};
pub use plan::{
    MOUNT_FINGERPRINT_FORMAT, ResolvedReference, build_snapshot, model_visible_references,
    mount_fingerprint, render_model_manifest,
};
pub use policy::{
    ReferenceDeploymentPolicy, SourceTrustRequirement, ValidatedSourceRoot,
    validate_host_path_syntax, validate_source_root,
};
pub use source::{
    ProvenanceCorroboration, SourceAvailability, SourceIdentity, ValidatedReferenceSource,
    provenance_corroboration, ready_source_record, revalidate_source, validate_new_source,
    validating_source_record, validation_failure,
};
pub use tree::{SourceEntryKind, SourceTreeEntry, SourceTreeScan, scan_source_tree};
