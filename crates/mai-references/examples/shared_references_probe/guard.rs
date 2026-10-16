//! 隔离边界：独立 Docker daemon、固定验收根与生产路径拒绝。
//!
//! 三条硬边界，任何一条不满足都 fail closed（非零退出），不用“先跑起来”为理由放宽：
//! 1. `DOCKER_HOST` 必须是指向独立验收 daemon 的 unix socket；任何其它取值（生产
//!    `/var/run/docker.sock`、`tcp://`、`ssh://`、空值）都拒绝，并且比对 inode，
//!    防止验收 socket 只是生产 socket 的别名。
//! 2. 验收根必须是固定验收目录；任何含 `proddata` 组件的路径、生产数据目录
//!    （`.mai-team`）或文件系统根都拒绝。
//! 3. 夹具与证据只能落在验收根内；Linux 源码只能是管理员已发布的只读快照路径。

use std::path::{Component, Path, PathBuf};

use crate::config::{ACCEPTANCE_DOCKER_SOCKET, ACCEPTANCE_ROOT, ProbeConfig};
use crate::error::{ProbeError, Result, err};

/// 生产 Docker daemon 的默认 socket；验收 socket 与它必须是不同的 inode。
const PRODUCTION_DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// 不允许出现在验收路径组件里的生产数据目录名。
const PRODUCTION_PATH_COMPONENTS: [&str; 3] = ["proddata", ".mai-team", "prod-data"];

/// 通过隔离检查后得到的 Docker 连接目标。
#[derive(Debug, Clone)]
pub struct DockerTarget {
    /// 规范的 `unix://` 形式，写进证据。
    pub host: String,
    /// socket 路径。
    pub socket: PathBuf,
    /// docker CLI 路径（可能是 root 提供的故障注入 wrapper）。
    pub binary: String,
}

/// 校验验收根、夹具路径与 Linux 源路径都在允许的隔离范围内。
pub fn ensure_isolated_root(config: &ProbeConfig) -> Result<()> {
    let expected = Path::new(ACCEPTANCE_ROOT);
    if config.root.as_path() != expected {
        return err(
            "acceptance_root_mismatch",
            format!(
                "验收根必须是固定目录 {}；实际为 {}。驱动不接受其它根（生产数据目录、proddata 路径一律拒绝）",
                expected.display(),
                config.root.display()
            ),
        );
    }
    ensure_plain_absolute(&config.root, "验收根")?;
    for (role, path) in [
        ("验收根", &config.root),
        ("数据目录", &config.data_dir),
        ("证据目录", &config.evidence_dir),
        ("夹具允许根", &config.fixture_root),
        ("夹具源码", &config.fixture_repo),
        ("部署策略文件", &config.policy_path),
    ] {
        ensure_not_production_path(path, role)?;
        if !path.starts_with(expected) {
            return err(
                "path_outside_acceptance_root",
                format!("{role} 必须位于验收根内：{}", path.display()),
            );
        }
    }
    for extra in &config.extra_allowed_roots {
        ensure_not_production_path(extra, "追加的允许根")?;
    }

    ensure_not_production_path(&config.linux_source, "Linux 快照源码")?;
    ensure_plain_absolute(&config.linux_source, "Linux 快照源码")?;
    let published_root = Path::new("/mnt/docker_nvme/mai-references");
    if !config.linux_source.starts_with(published_root) {
        return err(
            "linux_source_outside_published_root",
            format!(
                "Linux 快照源码必须位于管理员发布目录 {} 之下：{}",
                published_root.display(),
                config.linux_source.display()
            ),
        );
    }
    Ok(())
}

/// 拒绝生产数据目录、`proddata` 组件、文件系统根与相对/歧义路径。
pub fn ensure_not_production_path(path: &Path, role: &str) -> Result<()> {
    ensure_plain_absolute(path, role)?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = name.to_string_lossy().to_ascii_lowercase();
        if PRODUCTION_PATH_COMPONENTS
            .iter()
            .any(|banned| name == *banned)
        {
            return err(
                "production_path_rejected",
                format!("{role} 命中生产数据目录组件 `{name}`：{}", path.display()),
            );
        }
    }
    Ok(())
}

fn ensure_plain_absolute(path: &Path, role: &str) -> Result<()> {
    if !path.is_absolute() {
        return err(
            "path_not_absolute",
            format!("{role} 必须是绝对路径：{}", path.display()),
        );
    }
    if path == Path::new("/") {
        return err(
            "path_is_filesystem_root",
            format!("{role} 不能是文件系统根：{}", path.display()),
        );
    }
    for component in path.components() {
        match component {
            Component::ParentDir | Component::CurDir => {
                return err(
                    "path_not_normalized",
                    format!("{role} 不能包含 . 或 .. 组件：{}", path.display()),
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// 校验 `DOCKER_HOST` 指向独立验收 daemon，且它不是生产 socket 的别名。
pub fn ensure_isolated_docker_host(config: &ProbeConfig) -> Result<DockerTarget> {
    let raw = std::env::var("DOCKER_HOST")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ProbeError::new(
                "docker_host_not_set",
                format!(
                    "必须显式设置 DOCKER_HOST=unix://{ACCEPTANCE_DOCKER_SOCKET}；缺省值会让驱动连到生产 daemon"
                ),
            )
        })?;

    let socket_text = raw.strip_prefix("unix://").unwrap_or(raw.as_str()).trim();
    if !socket_text.starts_with('/') {
        return err(
            "docker_host_not_unix_socket",
            format!("DOCKER_HOST `{raw}` 不是本机 unix socket 路径；远程/tcp/ssh daemon 一律拒绝"),
        );
    }
    let expected = Path::new(ACCEPTANCE_DOCKER_SOCKET);
    if Path::new(socket_text) != expected {
        return err(
            "docker_host_not_acceptance_daemon",
            format!(
                "DOCKER_HOST `{raw}` 不是独立验收 daemon 的 socket（要求 {ACCEPTANCE_DOCKER_SOCKET}）"
            ),
        );
    }
    if let Some(context) = std::env::var("DOCKER_CONTEXT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        && context != "default"
    {
        return err(
            "docker_context_not_default",
            format!("DOCKER_CONTEXT=`{context}` 可能指向另一个 daemon；请显式使用缺省 context"),
        );
    }
    if !socket_exists(expected) {
        return err(
            "acceptance_docker_socket_missing",
            format!("独立验收 daemon socket 不存在：{}", expected.display()),
        );
    }
    if socket_exists(Path::new(PRODUCTION_DOCKER_SOCKET)) {
        let production = std::fs::metadata(PRODUCTION_DOCKER_SOCKET)?;
        let acceptance = std::fs::metadata(expected)?;
        if same_file_identity(&production, &acceptance) {
            return err(
                "acceptance_socket_aliases_production",
                format!(
                    "验收 socket {} 与生产 socket {PRODUCTION_DOCKER_SOCKET} 是同一个文件",
                    expected.display()
                ),
            );
        }
    }
    Ok(DockerTarget {
        host: format!("unix://{ACCEPTANCE_DOCKER_SOCKET}"),
        socket: expected.to_path_buf(),
        binary: config.docker_binary.clone(),
    })
}

/// 建立目录（幂等），并把新建目录的权限收敛到 0755，避免 umask 让可信归属检查失败。
pub fn ensure_directory(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    // 只收敛“本次调用前不存在”的目录：绝不修改既有目录的权限。
    let missing = path
        .ancestors()
        .filter(|ancestor| !ancestor.exists())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    std::fs::create_dir_all(path).map_err(|error| {
        ProbeError::new(
            "directory_create_failed",
            format!("无法创建目录 {}：{error}", path.display()),
        )
    })?;
    for ancestor in missing.into_iter().rev() {
        harden_directory(&ancestor)?;
    }
    Ok(())
}

/// 把目录权限设置为 0755（非 unix 平台不做任何事）。
#[cfg(unix)]
pub fn harden_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(path, permissions).map_err(|error| {
        ProbeError::new(
            "directory_permission_failed",
            format!("无法设置目录权限 {}：{error}", path.display()),
        )
    })
}

#[cfg(not(unix))]
pub fn harden_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn socket_exists(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn socket_exists(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}
