mod args;
mod capability;
mod capture;
mod client;
mod container;
mod copy;
mod environment;
mod error;
mod exec;
mod image;
mod inspect;
mod mount;
pub mod naming;
mod selection;

pub use args::ContainerCreateOptions;
pub use capability::{
    DockerCapabilities, MINIMUM_DOCKER_VERSION, MINIMUM_KERNEL_VERSION, is_unsupported_docker_host,
    meets_minimum_version, parse_version,
};
pub use capture::{
    ContainerTerminationOutcome, ExecCaptureFailure, ExecCaptureStream, ExecTerminationOutcome,
    MAX_EXEC_CAPTURE_BYTES, OutputLimitExceeded,
};
pub use client::DockerClient;
pub use container::ContainerHandle;
pub use copy::WorkspaceExportRequest;
pub use environment::{
    AgentEnvironmentPlan, AgentEnvironmentPlanBuilder, AgentMcpSidecarPlan, ContainerNetworkMode,
    EnvironmentDiff, EnvironmentDiffEntry, EnvironmentSnapshotReceipt, EnvironmentSnapshotRequest,
    ExistingEnvironment, VerifiedContainer, WorkspaceMountPlan, compare_environment,
    compare_required_mounts, parse_cpu_count, parse_docker_size, preserved_volume_mounts,
};
pub use error::{DockerError, Result};
pub use exec::{CapturedExecOutput, ExecCaptureOptions, ExecOutput, SidecarParams};
pub use image::{ImageRefreshOutcome, SnapshotImageFacts};
pub use inspect::{
    ContainerEnvironmentFacts, ContainerHostFacts, ContainerMountFacts, ManagedContainer,
    ManagedVolume,
};
pub use mount::{
    ContainerMount, ContainerMountAccess, ContainerMountType, ContainerReferencePath,
    ContainerVolumeMount, REFERENCE_MOUNT_ROOT, ReadOnlyBindMount, ValidatedHostDirectory,
};
pub use naming::{agent_workspace_volume, project_agent_workspace_volume, project_cache_volume};
pub use selection::ContainerRemovalPolicy;

mod snapshot_resources;
pub use snapshot_resources::{
    RemovedSnapshotImage, RetainedSnapshotImage, SnapshotCleanupReport, SnapshotOrigin,
    SnapshotOwnershipGap, SnapshotRetention, SnapshotRetentionReason,
};
