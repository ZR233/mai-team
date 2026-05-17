use crate::error::{DockerError, Result};
use crate::naming::{
    MANAGED_LABEL, PROJECT_LABEL_KEY, PROJECT_SIDECAR_KIND, SIDECAR_KIND_LABEL_KEY,
    SIDECAR_LABEL_KEY,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerCreateOptions {
    pub memory: Option<String>,
    pub cpus: Option<String>,
    pub pids_limit: Option<u32>,
    pub cap_drop_all: bool,
    pub no_new_privileges: bool,
    pub network: Option<String>,
}

#[cfg(test)]
pub(crate) fn create_agent_container_args(
    name: &str,
    agent_label: &str,
    image: &str,
    workspace_volume: &str,
) -> Vec<String> {
    create_agent_container_args_with_workspace(name, agent_label, image, workspace_volume)
}

pub(crate) fn create_agent_container_args_with_workspace(
    name: &str,
    agent_label: &str,
    image: &str,
    workspace_volume: &str,
) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        MANAGED_LABEL.to_string(),
        "--label".to_string(),
        agent_label.to_string(),
        "-v".to_string(),
        format!("{workspace_volume}:/workspace"),
        "--user".to_string(),
        "root".to_string(),
    ];
    args.extend([
        "-w".to_string(),
        "/workspace/repo".to_string(),
        image.to_string(),
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
        "/workspace".to_string(),
    ];
    apply_container_create_options(&mut args, options);
    args.extend([
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
        image.to_string(),
        "sleep".to_string(),
        "infinity".to_string(),
    ]
}

fn apply_container_create_options(args: &mut Vec<String>, options: &ContainerCreateOptions) {
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
    if let Some(network) = options
        .network
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.extend(["--network".to_string(), network.to_string()]);
    }
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
    use crate::naming::MANAGED_LABEL;

    #[test]
    fn create_agent_container_args_include_labels_workspace_and_image() {
        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let args = create_agent_container_args(
            "mai-team-child",
            "mai.team.agent=child",
            image,
            "mai-team-workspace-child",
        );

        assert_eq!(args[0], "create");
        assert!(
            args.windows(2)
                .any(|window| window == ["--name", "mai-team-child"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", MANAGED_LABEL])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--label", "mai.team.agent=child"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["-v", "mai-team-workspace-child:/workspace"])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["-w", "/workspace/repo"])
        );
        assert!(args.windows(2).any(|window| window == ["--user", "root"]));
        assert!(
            !args
                .windows(2)
                .any(|window| { window[0] == "-v" && window[1].ends_with(":/workspace/repo") })
        );
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
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
        assert!(args.windows(2).any(|window| window == ["-w", "/workspace"]));
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn create_workspace_copy_container_args_mount_workspace_volume() {
        let image = "ghcr.io/zr233/mai-team-sidecar:latest";
        let args = create_workspace_copy_container_args(
            "mai-team-project-skill-copy-project-1",
            image,
            "mai-team-project-review-project-1",
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
        assert!(
            args.windows(2)
                .any(|window| window == ["-v", "mai-team-project-review-project-1:/workspace"])
        );
        assert!(args.windows(2).any(|window| window == ["-w", "/workspace"]));
        assert!(
            args.windows(3)
                .any(|window| { window == [image, "sleep", "infinity"] })
        );
    }

    #[test]
    fn reviewer_agent_container_args_use_agent_workspace_volume() {
        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let args = create_agent_container_args(
            "mai-team-reviewer",
            "mai.team.agent=reviewer",
            image,
            "mai-team-project-project-1-agent-reviewer",
        );

        assert!(
            args.windows(2)
                .any(|window| window
                    == ["-v", "mai-team-project-project-1-agent-reviewer:/workspace"])
        );
        assert!(
            !args
                .windows(2)
                .any(|window| window == ["-v", "mai-team-project-review-project-1:/workspace"])
        );
    }

    #[test]
    fn project_agent_container_args_do_not_bind_host_repo_worktree() {
        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let args = create_agent_container_args_with_workspace(
            "mai-team-maintainer",
            "mai.team.agent=maintainer",
            image,
            "mai-team-project-project-1-agent-maintainer",
        );

        assert!(args.windows(2).any(|window| window
            == [
                "-v",
                "mai-team-project-project-1-agent-maintainer:/workspace"
            ]));
        assert!(!args.iter().any(|arg| arg.contains("/data/projects")));
        assert!(
            args.windows(2)
                .any(|window| window == ["-w", "/workspace/repo"])
        );
    }

    #[test]
    fn project_agent_container_args_run_as_root() {
        let image = "ghcr.io/rcore-os/tgoskits-container:latest";
        let args = create_agent_container_args_with_workspace(
            "mai-team-maintainer",
            "mai.team.agent=maintainer",
            image,
            "mai-team-project-project-1-agent-maintainer",
        );

        assert!(args.windows(2).any(|window| window == ["--user", "root"]));
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
                network: Some("mai-team".to_string()),
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
                .any(|window| window == ["--network", "mai-team"])
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
