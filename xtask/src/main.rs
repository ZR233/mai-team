use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match (args.next().as_deref(), args.next().as_deref(), args.next()) {
        (Some("run-remote"), Some("relay"), None) => run_remote_relay(),
        _ => {
            eprintln!("usage: cargo xtask run-remote relay");
            bail!("unknown xtask command");
        }
    }
}

fn run_remote_relay() -> Result<()> {
    let workspace_root = workspace_root()?;
    let remote = required_env("MAI_RELAY_REMOTE")?;
    if remote == "name@address" {
        bail!(
            "MAI_RELAY_REMOTE is still the placeholder `name@address`; set it in .cargo/config.toml or the environment"
        );
    }
    let relay_token = required_env("MAI_RELAY_TOKEN")?;
    if relay_token == "dev-relay-token-change-me" {
        bail!(
            "MAI_RELAY_TOKEN is still the placeholder `dev-relay-token-change-me`; set a private test token"
        );
    }

    println!("building relay binary");
    let relay_bin = build_relay(&workspace_root)?;
    let relay_bin = relay_bin
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", relay_bin.display()))?;

    let remote_bin = remote_path_for(&relay_bin)?;
    let remote_dir = parent_str(&remote_bin)?;
    let remote_db_path = format!("{remote_dir}/mai-relay.sqlite3");
    let remote_env_file = format!("{remote_dir}/mai-relay.env");
    let remote_pid_file = format!("{remote_dir}/mai-relay.pid");
    let cleanup_command = remote_cleanup_command(&remote_env_file, &remote_pid_file);

    println!("built relay binary: {}", relay_bin.display());
    println!("uploading to {remote}:{remote_bin}");
    run_status(
        Command::new("ssh")
            .arg(&remote)
            .arg(format!("mkdir -p {}", sh_quote(remote_dir))),
    )
    .context("creating remote relay directory")?;
    run_remote_cleanup(&remote, &cleanup_command).context("stopping previous remote relay")?;
    upload_remote_env(&remote, &remote_env_file, &remote_db_path)
        .context("uploading remote relay environment")?;
    upload_remote_binary(&remote, &relay_bin, &remote_bin)
        .context("copying relay binary to remote host")?;

    let remote_command = remote_relay_command(&remote_bin, &remote_env_file, &remote_pid_file)?;
    println!("starting remote relay over ssh; stop this command to stop the remote process");
    run_ssh_foreground(&remote, &remote_command, &cleanup_command)
}

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow!("xtask manifest has no parent directory"))?
        .to_path_buf())
}

fn build_relay(workspace_root: &Path) -> Result<PathBuf> {
    let plan = RelayBuildPlan::remote_test(workspace_root, relay_compile_env()?);
    run_status(&mut plan.command()).context("building mai-relay")?;

    Ok(plan.binary_path)
}

struct RelayBuildPlan {
    workspace_root: PathBuf,
    binary_path: PathBuf,
    features: Vec<String>,
    env: Vec<(String, String)>,
}

impl RelayBuildPlan {
    fn new(workspace_root: &Path) -> Self {
        Self {
            workspace_root: workspace_root.to_path_buf(),
            binary_path: workspace_root.join("target/release/mai-relay"),
            features: Vec::new(),
            env: Vec::new(),
        }
    }

    fn remote_test(workspace_root: &Path, env: Vec<(String, String)>) -> Self {
        Self {
            features: vec!["compiled-github-app-config".to_string()],
            env,
            ..Self::new(workspace_root)
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("cargo");
        command
            .arg("build")
            .arg("-p")
            .arg("mai-relay")
            .arg("--bin")
            .arg("mai-relay")
            .arg("--release")
            .current_dir(&self.workspace_root);
        if !self.features.is_empty() {
            command.arg("--features").arg(self.features.join(","));
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }
}

fn relay_compile_env() -> Result<Vec<(String, String)>> {
    let mut values = Vec::new();
    let app_id = required_env("MAI_RELAY_GITHUB_APP_ID")?;
    values.push(("MAI_RELAY_GITHUB_APP_ID".to_string(), app_id));
    values.push((
        "MAI_RELAY_GITHUB_APP_PRIVATE_KEY".to_string(),
        relay_private_key_for_compile()?,
    ));
    for name in [
        "MAI_RELAY_GITHUB_APP_SLUG",
        "MAI_RELAY_GITHUB_APP_HTML_URL",
        "MAI_RELAY_GITHUB_APP_OWNER_LOGIN",
        "MAI_RELAY_GITHUB_APP_OWNER_TYPE",
    ] {
        if let Ok(value) = env::var(name)
            && !is_placeholder_env(name, &value)
            && !value.trim().is_empty()
        {
            values.push((name.to_string(), value));
        }
    }
    Ok(values)
}

fn relay_private_key_for_compile() -> Result<String> {
    if let Ok(value) = env::var("MAI_RELAY_GITHUB_APP_PRIVATE_KEY")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    let path = required_env("MAI_RELAY_GITHUB_APP_PRIVATE_KEY_PATH")?;
    std::fs::read_to_string(&path)
        .with_context(|| format!("reading MAI_RELAY_GITHUB_APP_PRIVATE_KEY_PATH {path}"))
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} must be set"))
}

fn remote_path_for(local_abs_path: &Path) -> Result<String> {
    let local = local_abs_path.to_str().ok_or_else(|| {
        anyhow!(
            "local path is not valid UTF-8: {}",
            local_abs_path.display()
        )
    })?;
    Ok(format!("/tmp/mai-relay{local}"))
}

fn parent_str(path: &str) -> Result<&str> {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
        .ok_or_else(|| anyhow!("remote path has no parent: {path}"))
}

fn upload_remote_env(remote: &str, remote_env_file: &str, remote_db_path: &str) -> Result<()> {
    let mut env_file = String::new();
    for name in [
        "MAI_RELAY_TOKEN",
        "MAI_RELAY_PUBLIC_URL",
        "MAI_RELAY_BIND_ADDR",
        "MAI_RELAY_DB_PATH",
        "GITHUB_API_BASE_URL",
        "GITHUB_WEB_BASE_URL",
    ] {
        if let Ok(value) = env::var(name) {
            if is_placeholder_env(name, &value) {
                continue;
            }
            env_file.push_str(name);
            env_file.push('=');
            env_file.push_str(&sh_quote(&value));
            env_file.push('\n');
        }
    }
    if env::var_os("MAI_RELAY_BIND_ADDR").is_none() {
        env_file.push_str("MAI_RELAY_BIND_ADDR='0.0.0.0:8090'\n");
    }
    if env::var_os("MAI_RELAY_DB_PATH").is_none() {
        env_file.push_str("MAI_RELAY_DB_PATH=");
        env_file.push_str(&sh_quote(remote_db_path));
        env_file.push('\n');
    }

    let mut child = Command::new("ssh")
        .arg(remote)
        .arg(format!("umask 077; cat > {}", sh_quote(remote_env_file)))
        .stdin(Stdio::piped())
        .spawn()
        .context("starting ssh to upload relay env")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open ssh stdin"))?;
    stdin
        .write_all(env_file.as_bytes())
        .context("writing relay env to ssh stdin")?;
    drop(stdin);
    let status = child.wait().context("waiting for relay env upload")?;
    check_status("ssh", status)
}

fn upload_remote_binary(remote: &str, local_bin: &Path, remote_bin: &str) -> Result<()> {
    let temp_remote_bin = format!("{remote_bin}.upload-{}", std::process::id());
    run_status(
        Command::new("scp")
            .arg(local_bin)
            .arg(format!("{remote}:{temp_remote_bin}")),
    )?;
    run_status(Command::new("ssh").arg(remote).arg(format!(
        "set -e; chmod +x {tmp}; mv -f {tmp} {bin}",
        tmp = sh_quote(&temp_remote_bin),
        bin = sh_quote(remote_bin)
    )))
}

fn remote_relay_command(
    remote_bin: &str,
    remote_env_file: &str,
    remote_pid_file: &str,
) -> Result<String> {
    let remote_dir = parent_str(remote_bin)?;
    Ok(format!(
        "set -e; cd {dir}; set -a; . {env_file}; set +a; rm -f {env_file}; cleanup() {{ trap - INT TERM HUP EXIT; if [ -n \"${{pid:-}}\" ]; then kill -TERM \"$pid\" 2>/dev/null || true; wait \"$pid\" 2>/dev/null || true; fi; rm -f {pid_file} {env_file}; }}; trap cleanup INT TERM HUP EXIT; {bin} & pid=$!; printf '%s\\n' \"$pid\" > {pid_file}; wait \"$pid\"",
        dir = sh_quote(remote_dir),
        env_file = sh_quote(remote_env_file),
        pid_file = sh_quote(remote_pid_file),
        bin = sh_quote(remote_bin),
    ))
}

fn remote_cleanup_command(remote_env_file: &str, remote_pid_file: &str) -> String {
    format!(
        "if [ -f {pid_file} ]; then pid=$(cat {pid_file}); if [ -n \"$pid\" ]; then kill -TERM \"$pid\" 2>/dev/null || true; for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do kill -0 \"$pid\" 2>/dev/null || break; sleep 0.1; done; kill -KILL \"$pid\" 2>/dev/null || true; fi; fi; rm -f {pid_file} {env_file}",
        pid_file = sh_quote(remote_pid_file),
        env_file = sh_quote(remote_env_file),
    )
}

fn run_ssh_foreground(remote: &str, remote_command: &str, cleanup_command: &str) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    install_ctrlc_handler(Arc::clone(&interrupted))?;

    let mut child = Command::new("ssh")
        .arg(remote)
        .arg(remote_command)
        .spawn()
        .context("starting ssh")?;

    loop {
        if interrupted.load(Ordering::SeqCst) {
            let _ = run_remote_cleanup(remote, cleanup_command);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if child.try_wait().context("checking ssh status")?.is_some() {
                    println!("remote relay stopped");
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
            println!("remote relay stopped");
            return Ok(());
        }
        if let Some(status) = child.try_wait().context("checking ssh status")? {
            return check_status("ssh", status);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn run_remote_cleanup(remote: &str, cleanup_command: &str) -> Result<()> {
    let status = Command::new("ssh")
        .arg(remote)
        .arg(cleanup_command)
        .stdin(Stdio::null())
        .status()
        .context("running remote relay cleanup")?;
    check_status("ssh", status)
}

fn install_ctrlc_handler(interrupted: Arc<AtomicBool>) -> Result<()> {
    ctrlc::set_handler(move || {
        interrupted.store(true, Ordering::SeqCst);
    })
    .context("installing Ctrl-C handler")
}

fn run_status(command: &mut Command) -> Result<()> {
    let program = command.get_program().to_owned();
    let status = command
        .status()
        .with_context(|| format!("running {}", display_os(&program)))?;
    check_status(&display_os(&program), status)
}

fn check_status(program: &str, status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    match status.code() {
        Some(code) => bail!("{program} exited with status {code}"),
        None => bail!("{program} was terminated by signal"),
    }
}

fn sh_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn display_os(value: &OsStr) -> String {
    PathBuf::from(OsString::from(value)).display().to_string()
}

fn is_placeholder_env(name: &str, value: &str) -> bool {
    matches!(
        (name, value),
        ("MAI_RELAY_GITHUB_APP_ID", "github-app-id")
            | (
                "MAI_RELAY_GITHUB_APP_PRIVATE_KEY_PATH",
                "/absolute/path/to/github-app.private-key.pem"
            )
            | ("MAI_RELAY_GITHUB_APP_SLUG", "github-app-slug")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_build_plan_uses_release_profile() {
        let workspace_root = PathBuf::from("/workspace");
        let plan = RelayBuildPlan::remote_test(
            &workspace_root,
            vec![("MAI_RELAY_GITHUB_APP_ID".to_string(), "123".to_string())],
        );
        let command = plan.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "build".to_string(),
                "-p".to_string(),
                "mai-relay".to_string(),
                "--bin".to_string(),
                "mai-relay".to_string(),
                "--release".to_string(),
                "--features".to_string(),
                "compiled-github-app-config".to_string(),
            ]
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "MAI_RELAY_GITHUB_APP_ID")
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned()),
            Some("123".to_string())
        );
        assert_eq!(
            plan.binary_path,
            workspace_root.join("target/release/mai-relay")
        );
    }
}
