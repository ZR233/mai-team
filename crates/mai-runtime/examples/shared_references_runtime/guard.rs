//! 隔离边界：固定验收根与独立验收 daemon，任何一条不满足都 fail closed。
//!
//! 三条硬边界：
//! 1. `DOCKER_HOST` 必须**恰好**是独立验收 daemon 的 unix socket；其它取值（生产
//!    `/var/run/docker.sock`、`tcp://`、`ssh://`、空值）一律拒绝，并且比对 inode，
//!    防止验收 socket 只是生产 socket 的别名。
//! 2. 验收根必须是固定目录；含 `proddata`/`.mai-team` 组件的路径与文件系统根都拒绝。
//! 3. 前一领域驱动 seed 的产品库、部署策略与两份源码必须真实存在；驱动不创建它们。

use std::path::{Component, Path, PathBuf};

use crate::error::{Result, error};
use crate::runtime_config::{
    ACCEPTANCE_DOCKER_HOST, ACCEPTANCE_DOCKER_SOCKET, ACCEPTANCE_ROOT, DriverConfig,
};

/// 生产 Docker daemon 的默认 socket；验收 socket 与它不能是同一个 inode。
const PRODUCTION_DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// 不允许出现在验收路径组件里的生产数据目录名。
const PRODUCTION_PATH_COMPONENTS: [&str; 3] = ["proddata", ".mai-team", "prod-data"];

/// 校验本次运行的隔离边界。
pub fn ensure_boundaries(config: &DriverConfig) -> Result<()> {
    ensure_fixed_root(config)?;
    ensure_isolated_docker_host()?;
    require_dir(&config.fixture_root, "夹具允许根")?;
    require_dir(&config.fixture_repo, "夹具源码（前一领域驱动的夹具）")?;
    require_dir(&config.linux_source, "已发布的 Linux 快照")?;
    require_file(&config.policy_path, "部署策略 reference-sources.toml")?;
    require_file(
        &config.db_path,
        "前一领域驱动 seed 的产品库 root/data/mai-team.sqlite3",
    )?;
    Ok(())
}

/// 非致命观察：缺这些不会让边界失效，但会限制某些场景（写进证据的 boundaries）。
pub fn boundary_notes(config: &DriverConfig) -> Vec<String> {
    let mut notes = Vec::new();
    if !config.provider_evidence_dir.is_dir() {
        notes.push(format!(
            "fixture provider 的请求日志目录不存在：{}；判定将只使用工具 trace 与容器事实，请用 --evidence-dir 启动 fixture provider 指向该目录",
            config.provider_evidence_dir.display()
        ));
    }
    if !config.system_agents_root.is_dir() {
        notes.push(format!(
            "system agent profile 目录不存在：{}；spawn_agent 需要它来解析被授权 Profile",
            config.system_agents_root.display()
        ));
    }
    if !config.system_skills_root.is_dir() {
        notes.push(format!(
            "system skill 目录不存在：{}；Thread 装配会用空技能目录继续",
            config.system_skills_root.display()
        ));
    }
    notes
}

fn ensure_fixed_root(config: &DriverConfig) -> Result<()> {
    let expected = Path::new(ACCEPTANCE_ROOT);
    if config.root.as_path() != expected {
        return Err(error(
            "acceptance_root_mismatch",
            format!(
                "验收根必须是固定目录 {}；实际为 {}。驱动不接受其它根（生产数据目录、proddata 路径一律拒绝）",
                expected.display(),
                config.root.display()
            ),
        ));
    }
    for (role, path) in [
        ("验收根", &config.root),
        ("数据目录", &config.data_dir),
        ("证据目录", &config.evidence_dir),
        ("夹具源码", &config.fixture_repo),
        ("部署策略", &config.policy_path),
    ] {
        ensure_plain_absolute(path, role)?;
        ensure_not_production_path(path, role)?;
        if !path.starts_with(expected) {
            return Err(error(
                "path_outside_acceptance_root",
                format!("{role} 必须位于验收根内：{}", path.display()),
            ));
        }
    }
    ensure_not_production_path(&config.linux_source, "Linux 快照源码")?;
    Ok(())
}

fn ensure_not_production_path(path: &Path, role: &str) -> Result<()> {
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = name.to_string_lossy().to_ascii_lowercase();
        if PRODUCTION_PATH_COMPONENTS
            .iter()
            .any(|banned| name == *banned)
        {
            return Err(error(
                "production_path_rejected",
                format!("{role} 命中生产数据目录组件 `{name}`：{}", path.display()),
            ));
        }
    }
    Ok(())
}

fn ensure_plain_absolute(path: &Path, role: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(error(
            "path_not_absolute",
            format!("{role} 必须是绝对路径：{}", path.display()),
        ));
    }
    if path == Path::new("/") {
        return Err(error(
            "path_is_filesystem_root",
            format!("{role} 不能是文件系统根：{}", path.display()),
        ));
    }
    for component in path.components() {
        if matches!(component, Component::CurDir | Component::ParentDir) {
            return Err(error(
                "path_not_normalized",
                format!("{role} 不能包含 . 或 .. 组件：{}", path.display()),
            ));
        }
    }
    Ok(())
}

fn ensure_isolated_docker_host() -> Result<()> {
    let raw = std::env::var("DOCKER_HOST")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            error(
                "docker_host_not_set",
                format!("必须显式设置 DOCKER_HOST={ACCEPTANCE_DOCKER_HOST}；缺省值会让驱动连到生产 daemon"),
            )
        })?;
    if raw != ACCEPTANCE_DOCKER_HOST {
        return Err(error(
            "docker_host_not_acceptance_daemon",
            format!(
                "DOCKER_HOST 必须是 {ACCEPTANCE_DOCKER_HOST}（实际 `{raw}`）；远程/tcp/ssh daemon 一律拒绝"
            ),
        ));
    }
    // 只比路径不够：验收 socket 可能是生产 socket 的符号链接/别名。
    let acceptance = std::fs::metadata(ACCEPTANCE_DOCKER_SOCKET).map_err(|cause| {
        error(
            "acceptance_socket_missing",
            format!("验收 daemon socket 不可访问 {ACCEPTANCE_DOCKER_SOCKET}：{cause}"),
        )
    })?;
    if let Ok(production) = std::fs::metadata(PRODUCTION_DOCKER_SOCKET)
        && same_file(&acceptance, &production)
    {
        return Err(error(
            "acceptance_socket_is_production_alias",
            format!(
                "验收 socket {ACCEPTANCE_DOCKER_SOCKET} 与 {PRODUCTION_DOCKER_SOCKET} 是同一个文件"
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

fn require_dir(path: &Path, role: &str) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|cause| {
        error(
            "required_path_missing",
            format!("{role} 不存在或不可读：{}（{cause}）", path.display()),
        )
    })?;
    if !metadata.is_dir() {
        return Err(error(
            "required_path_not_directory",
            format!("{role} 不是目录：{}", path.display()),
        ));
    }
    Ok(())
}

fn require_file(path: &Path, role: &str) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|cause| {
        error(
            "required_path_missing",
            format!("{role} 不存在或不可读：{}（{cause}）", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(error(
            "required_path_not_file",
            format!("{role} 不是普通文件：{}", path.display()),
        ));
    }
    Ok(())
}

/// 证据里使用的路径清单。
pub fn path_facts(config: &DriverConfig) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("root", config.root.clone()),
        ("data_dir", config.data_dir.clone()),
        ("db_path", config.db_path.clone()),
        ("evidence_dir", config.evidence_dir.clone()),
        (
            "provider_evidence_dir",
            config.provider_evidence_dir.clone(),
        ),
        ("policy_path", config.policy_path.clone()),
        ("fixture_repo", config.fixture_repo.clone()),
        ("linux_source", config.linux_source.clone()),
        ("system_agents_root", config.system_agents_root.clone()),
    ]
}
