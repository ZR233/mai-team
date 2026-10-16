use uuid::Uuid;

use crate::error::{DockerError, Result};
use crate::mount::ContainerMount;
use crate::naming::{
    AGENT_LABEL_KEY, AGENT_MCP_SIDECAR_KIND, GENERATION_LABEL_KEY, MANAGED_LABEL,
    PROJECT_LABEL_KEY, PROJECT_SIDECAR_KIND, SIDECAR_KIND_LABEL_KEY, SIDECAR_LABEL_KEY,
};
use crate::{AgentEnvironmentPlan, ContainerNetworkMode};

pub(crate) const HOST_NETWORK: &str = "host";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerCreateOptions {
    pub memory: Option<String>,
    pub cpus: Option<String>,
    pub pids_limit: Option<u32>,
    pub cap_drop_all: bool,
    pub no_new_privileges: bool,
}

pub(crate) fn default_agent_container_options() -> ContainerCreateOptions {
    ContainerCreateOptions {
        memory: Some("8g".to_string()),
        cpus: Some("4".to_string()),
        pids_limit: Some(2048),
        cap_drop_all: false,
        no_new_privileges: false,
    }
}

/// 由最终环境计划生成 `docker create` 的 argv。
///
/// 工作区、附加挂载与保留卷统一通过 `--mount` 表达，因此 inspect 核对与创建参数
/// 来自同一份 mount 集合；参数始终是独立 argv，不经过 shell。
pub(crate) fn create_agent_environment_args(
    plan: &AgentEnvironmentPlan,
    mounts: &[ContainerMount],
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--name".to_string(),
        plan.container_name(),
        "--label".to_string(),
        MANAGED_LABEL.to_string(),
        "--label".to_string(),
        plan.agent_label_value(),
    ];
    if let Some(project_id) = plan.project_id() {
        args.extend([
            "--label".to_string(),
            format!("{PROJECT_LABEL_KEY}={project_id}"),
        ]);
    }
    if let Some(label) = plan.generation_label_value() {
        args.extend(["--label".to_string(), label]);
    }
    args.extend(["--user".to_string(), "root".to_string()]);
    for mount in mounts {
        args.extend(["--mount".to_string(), mount.docker_mount_spec()]);
    }
    apply_container_create_options(&mut args, plan.options(), plan.network());
    args.extend([
        "-w".to_string(),
        "/workspace/repo".to_string(),
        plan.image_reference().to_string(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]);
    args
}

pub(crate) fn create_project_sidecar_container_args(
    name: &str,
    project_id: &str,
    image: &str,
    workspace_volume: &str,
    options: &ContainerCreateOptions,
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        MANAGED_LABEL.to_string(),
        "--label".to_string(),
        format!("{SIDECAR_LABEL_KEY}=true"),
        "--label".to_string(),
        format!("{SIDECAR_KIND_LABEL_KEY}={PROJECT_SIDECAR_KIND}"),
        "--label".to_string(),
        format!("{PROJECT_LABEL_KEY}={project_id}"),
        "-v".to_string(),
        format!("{workspace_volume}:/workspace"),
        "-w".to_string(),
        "/workspace/repo".to_string(),
    ];
    apply_container_create_options(&mut args, options, ContainerNetworkMode::Host);
    args.extend([
        image.to_string(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]);
    args
}

/// MCP sidecar 的 argv：显式挂载与 Agent 相同的 typed 只读挂载。
///
/// `--volumes-from` 只携带卷，不携带 bind 挂载，因此参考源码必须显式重新声明，
/// 并且仍然使用同一套 `readonly,bind-recursive=readonly,bind-propagation=rprivate`。
pub(crate) fn create_agent_mcp_sidecar_container_args(
    name: &str,
    agent_id: &str,
    image: &str,
    source_container_id: &str,
    generation: Uuid,
    mounts: &[ContainerMount],
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        MANAGED_LABEL.to_string(),
        "--label".to_string(),
        format!("{SIDECAR_LABEL_KEY}=true"),
        "--label".to_string(),
        format!("{SIDECAR_KIND_LABEL_KEY}={AGENT_MCP_SIDECAR_KIND}"),
        "--label".to_string(),
        format!("{AGENT_LABEL_KEY}={agent_id}"),
        "--label".to_string(),
        format!("{GENERATION_LABEL_KEY}={generation}"),
        "--label".to_string(),
        format!("mai.team.sidecar.source={source_container_id}"),
        "--volumes-from".to_string(),
        source_container_id.to_string(),
        "--network".to_string(),
        HOST_NETWORK.to_string(),
    ];
    for mount in mounts {
        args.extend(["--mount".to_string(), mount.docker_mount_spec()]);
    }
    args.extend([
        "-w".to_string(),
        "/workspace/repo".to_string(),
        image.to_string(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]);
    args
}

pub(crate) fn create_workspace_copy_container_args(
    name: &str,
    image: &str,
    workspace_volume: &str,
) -> Vec<String> {
    vec![
        "create".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        MANAGED_LABEL.to_string(),
        "-v".to_string(),
        format!("{workspace_volume}:/workspace"),
        "-w".to_string(),
        "/workspace".to_string(),
        "--network".to_string(),
        HOST_NETWORK.to_string(),
        image.to_string(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]
}

fn apply_container_create_options(
    args: &mut Vec<String>,
    options: &ContainerCreateOptions,
    network: ContainerNetworkMode,
) {
    if let Some(memory) = options
        .memory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.extend(["--memory".to_string(), memory.to_string()]);
    }
    if let Some(cpus) = options
        .cpus
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.extend(["--cpus".to_string(), cpus.to_string()]);
    }
    if let Some(pids_limit) = options.pids_limit {
        args.extend(["--pids-limit".to_string(), pids_limit.to_string()]);
    }
    if options.cap_drop_all {
        args.extend(["--cap-drop".to_string(), "ALL".to_string()]);
    }
    if options.no_new_privileges {
        args.extend([
            "--security-opt".to_string(),
            "no-new-privileges".to_string(),
        ]);
    }
    args.extend([
        "--network".to_string(),
        network.as_docker_value().to_string(),
    ]);
}

pub(crate) fn validate_image(image: &str) -> Result<&str> {
    if image.trim().is_empty() {
        return Err(DockerError::InvalidImage(
            "image name cannot be empty".to_string(),
        ));
    }
    if image.trim() != image {
        return Err(DockerError::InvalidImage(
            "image name cannot include leading or trailing whitespace".to_string(),
        ));
    }
    if image.chars().any(char::is_whitespace) {
        return Err(DockerError::InvalidImage(
            "image name cannot include whitespace".to_string(),
        ));
    }
    if image.chars().any(char::is_control) {
        return Err(DockerError::InvalidImage(
            "image name cannot include control characters".to_string(),
        ));
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceMountPlan;
    use crate::mount::ContainerVolumeMount;
    use crate::naming::MANAGED_LABEL;

    #[test]
    fn create_agent_environment_args_include_labels_mounts_and_image() {
        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let plan = environment_plan(
            "agent-1",
            image,
            Some("project-1"),
            vec![reference_mount("linux")],
        );
        let mounts = plan.creation_mounts(&[]).expect("creation mounts");
        let args = create_agent_environment_args(&plan, &mounts);

        assert_eq!(args[0], "create");
        assert!(
            args.windows(2)
                .any(|window| window == ["--name", "mai-team-agent-1-g8f058410c69e"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", MANAGED_LABEL])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.agent=agent-1"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.project=project-1"])
        );
        assert!(args.windows(2).any(|window| window
            == [
                "--label",
                "mai.team.generation=8f058410-c69e-45d6-a10d-d204db72b30a"
            ]));
        assert!(args.windows(2).any(|window| window
            == [
                "--mount",
                "type=volume,src=mai-team-workspace-agent-1,dst=/workspace"
            ]));
        assert!(args.windows(2).any(|window| window
            == [
                "--mount",
                "type=bind,src=/srv/mai-references/linux/repo,dst=/references/linux,readonly,bind-recursive=readonly,bind-propagation=rprivate"
            ]));
        assert!(
            args.windows(2)
                .any(|window| window == ["-w", "/workspace/repo"])
        );
        assert!(args.windows(2).any(|window| window == ["--user", "root"]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--network", "host"])
        );
        assert!(args.windows(2).any(|window| window == ["--memory", "8g"]));
        assert!(args.windows(2).any(|window| window == ["--cpus", "4"]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--pids-limit", "2048"])
        );
        assert_eq!(
            args.windows(3)
                .find(|window| window[0] == image)
                .map(|window| window.to_vec()),
            Some(vec![
                image.to_string(),
                "sleep".to_string(),
                "infinity".to_string()
            ])
        );
        // 工作区只通过 mount 集合表达一次，避免两套参数来源。
        assert!(
            !args
                .windows(2)
                .any(|window| window[0] == "-v" && window[1].contains("/workspace"))
        );
        // args 始终是独立 argv：不借助 shell，也不包含 shell 元字符。
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains([';', '`', '&', '|', '$']) || arg.contains(' '))
        );
    }

    #[test]
    fn create_agent_environment_args_include_read_only_review_subpath_mount() {
        let plan = environment_plan(
            "reviewer-1",
            "mai-reviewer:latest",
            None,
            vec![ContainerMount::Volume(
                ContainerVolumeMount::read_only_subpath(
                    "mai-team-project-project-1-cache",
                    "/project/repo",
                    "review-contexts/run-1/repo",
                )
                .expect("valid reviewer mount"),
            )],
        );
        let mounts = plan.creation_mounts(&[]).expect("creation mounts");
        let args = create_agent_environment_args(&plan, &mounts);

        assert!(args.windows(2).any(|window| {
            window
                == [
                    "--mount",
                    "type=volume,src=mai-team-project-project-1-cache,dst=/project/repo,volume-subpath=review-contexts/run-1/repo,readonly",
                ]
        }));
        assert!(
            !args
                .windows(2)
                .any(|window| window[0] == "-v" && window[1].contains("project-review-"))
        );
    }

    fn environment_plan(
        agent_id: &str,
        image: &str,
        project_id: Option<&str>,
        mounts: Vec<ContainerMount>,
    ) -> AgentEnvironmentPlan {
        let builder = AgentEnvironmentPlan::builder(
            agent_id,
            image,
            WorkspaceMountPlan::agent_default(agent_id).expect("workspace"),
        )
        .effective_image_id("sha256:effective")
        .generation(Uuid::parse_str("8f058410-c69e-45d6-a10d-d204db72b30a").expect("uuid"))
        .mounts(mounts);
        match project_id {
            Some(project_id) => builder.project_id(project_id).build(),
            None => builder.build(),
        }
        .expect("plan")
    }

    fn reference_mount(alias: &str) -> ContainerMount {
        ContainerMount::ReadOnlyBind(
            crate::mount::ReadOnlyBindMount::for_reference("/srv/mai-references/linux/repo", alias)
                .expect("reference mount"),
        )
    }

    #[test]
    fn create_project_sidecar_container_args_include_labels_workspace_and_image() {
        let image = "ghcr.io/zr233/mai-team-sidecar:latest";
        let args = create_project_sidecar_container_args(
            "mai-team-project-sidecar-project-1",
            "project-1",
            image,
            "mai-team-project-project-1",
            &ContainerCreateOptions::default(),
        );

        assert_eq!(args[0], "create");
        assert!(
            args.windows(2)
                .any(|window| window == ["--name", "mai-team-project-sidecar-project-1"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", MANAGED_LABEL])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.sidecar=true"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.sidecar.kind=project"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.project=project-1"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["-v", "mai-team-project-project-1:/workspace"])
        );
        assert!(!args.windows(2).any(|window| window[0] == "-e"));
        assert!(
            args.windows(2)
                .any(|window| window == ["--network", "host"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["-w", "/workspace/repo"])
        );
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn create_agent_mcp_sidecar_args_share_agent_workspace_and_identity() {
        let image = "ghcr.io/zr233/mai-team-sidecar:latest";
        let args = create_agent_mcp_sidecar_container_args(
            "mai-team-agent-mcp-agent-1-g8f058410c69e",
            "agent-1",
            image,
            "agent-container-id",
            Uuid::parse_str("8f058410-c69e-45d6-a10d-d204db72b30a").expect("uuid"),
            &[reference_mount("linux")],
        );

        assert_eq!(args[0], "create");
        assert!(
            args.windows(2)
                .any(|window| window == ["--name", "mai-team-agent-mcp-agent-1-g8f058410c69e"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", MANAGED_LABEL])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.sidecar=true"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.sidecar.kind=agent-mcp"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.agent=agent-1"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--volumes-from", "agent-container-id"])
        );
        assert!(args.windows(2).any(|window| window
            == [
                "--label",
                "mai.team.generation=8f058410-c69e-45d6-a10d-d204db72b30a"
            ]));
        // sidecar 必须显式继承同一份严格只读参考挂载。
        assert!(args.windows(2).any(|window| window
            == [
                "--mount",
                "type=bind,src=/srv/mai-references/linux/repo,dst=/references/linux,readonly,bind-recursive=readonly,bind-propagation=rprivate"
            ]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--network", HOST_NETWORK])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["-w", "/workspace/repo"])
        );
        assert!(
            args.windows(3)
                .any(|window| window == [image, "sleep", "infinity"])
        );
    }

    #[test]
    fn create_workspace_copy_container_args_mount_workspace_volume() {
        let image = "ghcr.io/zr233/mai-team-sidecar:latest";
        let args = create_workspace_copy_container_args(
            "mai-team-project-skill-copy-project-1",
            image,
            "mai-team-project-project-1-agent-reviewer-1",
        );

        assert_eq!(args[0], "create");
        assert!(
            args.windows(2)
                .any(|window| window == ["--name", "mai-team-project-skill-copy-project-1"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", MANAGED_LABEL])
        );
        assert!(args.windows(2).any(|window| window
            == [
                "-v",
                "mai-team-project-project-1-agent-reviewer-1:/workspace"
            ]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--network", "host"])
        );
        assert!(args.windows(2).any(|window| window == ["-w", "/workspace"]));
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn create_project_sidecar_container_args_include_optional_hardening() {
        let image = "ghcr.io/zr233/mai-team-sidecar:latest";
        let args = create_project_sidecar_container_args(
            "mai-team-project-sidecar-project-1",
            "project-1",
            image,
            "mai-team-project-project-1",
            &ContainerCreateOptions {
                memory: Some("1g".to_string()),
                cpus: Some("2".to_string()),
                pids_limit: Some(100),
                cap_drop_all: true,
                no_new_privileges: true,
            },
        );

        assert!(args.windows(2).any(|window| window == ["--memory", "1g"]));
        assert!(args.windows(2).any(|window| window == ["--cpus", "2"]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--pids-limit", "100"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--cap-drop", "ALL"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--security-opt", "no-new-privileges"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--network", "host"])
        );
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn validate_image_rejects_empty_whitespace_and_control_characters() {
        assert_eq!(
            validate_image("ubuntu:latest").expect("valid"),
            "ubuntu:latest"
        );
        assert!(matches!(
            validate_image(""),
            Err(DockerError::InvalidImage(_))
        ));
        assert!(matches!(
            validate_image(" ubuntu:latest"),
            Err(DockerError::InvalidImage(_))
        ));
        assert!(matches!(
            validate_image("ubuntu latest"),
            Err(DockerError::InvalidImage(_))
        ));
        assert!(matches!(
            validate_image("ubuntu:\nlatest"),
            Err(DockerError::InvalidImage(_))
        ));
    }
}
