use std::path::Path;

use mai_protocol::{ProjectId, preview};
use tokio::process::Command;

use crate::{Result, RuntimeError};

pub(crate) mod docker_reconcile;
pub(crate) mod lease;
pub(crate) mod manager;
pub(crate) mod paths;
pub(crate) mod policy;
pub(crate) mod reconcile;

#[cfg(test)]
pub(crate) use manager::sync_project_repo_cache;
pub(crate) use manager::{
    AGENT_WORKSPACE_REPO_PATH, CloneSeed, LocalProjectWorkspaceManager, ProjectWorkspaceManager,
};
#[cfg(test)]
pub(crate) use paths::{agent_clone_path, project_repo_cache_path};

pub(crate) fn delete_project_workspace(projects_root: &Path, project_id: ProjectId) -> Result<()> {
    let _ = std::fs::remove_dir_all(paths::project_dir(projects_root, project_id));
    Ok(())
}

pub(crate) async fn git_plain<const N: usize>(
    git_binary: &str,
    cwd: &Path,
    args: [&str; N],
) -> Result<String> {
    run_git(git_binary, cwd, &args, None).await
}

pub(crate) async fn git_with_token<const N: usize>(
    git_binary: &str,
    cwd: &Path,
    token: &str,
    args: [&str; N],
) -> Result<String> {
    run_git(git_binary, cwd, &args, Some(token)).await
}

async fn run_git(
    git_binary: &str,
    cwd: &Path,
    args: &[&str],
    token: Option<&str>,
) -> Result<String> {
    let tmp;
    let askpass_path;
    if token.is_some() {
        tmp = tempfile::TempDir::new()?;
        askpass_path = tmp.path().join("askpass.sh");
        std::fs::write(
            &askpass_path,
            "#!/bin/sh\ncase \"$1\" in\n  *Username*) printf '%s\\n' x-access-token ;;\n  *Password*) printf '%s\\n' \"$MAI_GITHUB_INSTALLATION_TOKEN\" ;;\n  *) printf '\\n' ;;\nesac\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&askpass_path)?.permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&askpass_path, permissions)?;
        }
    } else {
        tmp = tempfile::TempDir::new()?;
        askpass_path = tmp.path().join("unused");
    }

    let mut command = Command::new(git_binary);
    command.current_dir(cwd).args(args);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "safe.directory")
        .env("GIT_CONFIG_VALUE_1", cwd)
        .env("GIT_CONFIG_KEY_2", "credential.helper")
        .env("GIT_CONFIG_VALUE_2", "")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS");
    if let Some(token) = token {
        command
            .env("GIT_ASKPASS", &askpass_path)
            .env("MAI_GITHUB_INSTALLATION_TOKEN", token);
    } else {
        command.env_remove("MAI_GITHUB_INSTALLATION_TOKEN");
    }
    let output = command.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let combined = redact_secret(
        format!("{stderr}\n{stdout}").trim(),
        token.unwrap_or_default(),
    );
    Err(RuntimeError::InvalidInput(format!(
        "git {} failed: {}",
        args.first().copied().unwrap_or("command"),
        preview(combined.trim(), 500)
    )))
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn git_plain_uses_host_git_safety_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_path = dir.path().join("fake-git.sh");
        let log_path = dir.path().join("git-env.log");
        std::fs::write(
            &git_path,
            format!(
                "#!/bin/sh\nprintf 'prompt=%s\\nno_system=%s\\nconfig_count=%s\\nhooks=%s\\nsafe=%s\\ncredential=%s\\n' \"$GIT_TERMINAL_PROMPT\" \"$GIT_CONFIG_NOSYSTEM\" \"$GIT_CONFIG_COUNT\" \"$GIT_CONFIG_VALUE_0\" \"$GIT_CONFIG_VALUE_1\" \"$GIT_CONFIG_VALUE_2\" > {}\n",
                shell_quote(&log_path.to_string_lossy())
            ),
        )
        .expect("write fake git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&git_path)
                .expect("fake git metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&git_path, permissions).expect("chmod fake git");
        }

        git_plain(&git_path.to_string_lossy(), dir.path(), ["status"])
            .await
            .expect("git status");

        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("prompt=0"));
        assert!(log.contains("no_system=1"));
        assert!(log.contains("config_count=3"));
        assert!(log.contains("hooks=/dev/null"));
        assert!(log.contains(&format!("safe={}", dir.path().display())));
        assert!(log.contains("credential="));
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
