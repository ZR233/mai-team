//! 容器内尝试原语：只读写入尝试、提权探测、参考文件定位与 capability 读取。

use super::*;

/// 一次容器内尝试的结果。
#[derive(Debug, Clone)]
pub struct AttemptResult {
    pub name: &'static str,
    pub status: i32,
}

impl AttemptResult {
    /// 判定这次尝试。
    pub fn verdict(&self) -> AttemptVerdict {
        if self.status == 127 {
            return AttemptVerdict::ToolMissing;
        }
        if self.status == 0 {
            AttemptVerdict::UnexpectedSuccess
        } else {
            AttemptVerdict::Rejected
        }
    }
}

/// 尝试的判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptVerdict {
    /// 按期望被拒绝（非零退出）。
    Rejected,
    /// 意外成功（零退出）。
    UnexpectedSuccess,
    /// 容器内缺少命令，无法判定。
    ToolMissing,
}

/// 在容器里跑一次尝试。
async fn attempt(
    probe: &Probe,
    container: &str,
    name: &'static str,
    command: String,
) -> Result<AttemptResult> {
    let output = environment::exec(probe, container, &command).await?;
    Ok(AttemptResult {
        name,
        status: output.status,
    })
}

/// 只读写入尝试；除 chmod 外都不写被保护目标本身（chmod 只在只读保护失效时才有副作用）。
pub async fn write_attempts(
    probe: &Probe,
    container: &str,
    directory: &str,
    file: &str,
) -> Result<Vec<AttemptResult>> {
    let directory = shell_quote(directory);
    let file = shell_quote(file);
    let attempts = vec![
        ("open-append-original", format!("exec 3>>{file}")),
        (
            "touch-new",
            format!("touch {directory}/acceptance-attempt-touch"),
        ),
        (
            "append-new",
            format!("printf x >> {directory}/acceptance-attempt-append"),
        ),
        (
            "rename-into-source",
            "printf x > /tmp/acceptance-rename-source && mv /tmp/acceptance-rename-source"
                .to_string()
                + &format!(" {directory}/acceptance-renamed"),
        ),
        ("chmod-original", format!("chmod 0600 {file}")),
        (
            "hardlink-original",
            format!("ln {file} /tmp/acceptance-hardlink"),
        ),
    ];
    let mut attempts = attempts;
    // 删除必须针对存在的夹具文件；rm -f 不存在文件成功不能证明可写。
    // 固定 Linux 快照不进行可能破坏原件的删除尝试。
    if directory.contains("private") {
        attempts.push(("remove-existing", format!("rm {file}")));
    }
    let mut results = Vec::new();
    for (name, command) in attempts {
        results.push(attempt(probe, container, name, command).await?);
    }
    Ok(results)
}

/// 提权与隔离探测（期望全部非零退出，包括 docker socket 不存在）。
pub(super) async fn privilege_attempts(
    probe: &Probe,
    container: &str,
    agent: &AgentFacts,
) -> Result<Vec<AttemptResult>> {
    let mut attempts = vec![(
        "docker-socket-absent",
        "test -S /var/run/docker.sock".to_string(),
    )];
    if let Some(reference) = agent
        .binding
        .as_ref()
        .and_then(|binding| binding.references.first())
    {
        let directory = shell_quote(&reference.container_path);
        attempts.push((
            "remount-readwrite",
            format!("mount -o remount,rw {directory}"),
        ));
        attempts.push(("umount-reference", format!("umount {directory}")));
    }
    let mut results = Vec::new();
    for (name, command) in attempts {
        results.push(attempt(probe, container, name, command).await?);
    }
    Ok(results)
}

/// 读取 `/proc/self/status` 的 CapEff 并判断是否含 CAP_SYS_ADMIN（bit 21）。
pub(super) async fn effective_sys_admin(probe: &Probe, container: &str) -> Result<Option<bool>> {
    let output = environment::exec(
        probe,
        container,
        "awk '/^CapEff/ {print $2}' /proc/self/status",
    )
    .await?;
    if output.status != 0 {
        return Ok(None);
    }
    let Some(value) = output.stdout.split_whitespace().next() else {
        return Ok(None);
    };
    let Ok(bits) = u64::from_str_radix(value, 16) else {
        return Ok(None);
    };
    Ok(Some(bits & (1 << 21) != 0))
}

/// 在参考目录里找一个普通文件，供读取与写入尝试使用。
pub async fn find_reference_file(
    probe: &Probe,
    container: &str,
    directory: &str,
) -> Result<Option<String>> {
    let directory = shell_quote(directory);
    let output = environment::exec(
        probe,
        container,
        &format!("find {directory} -maxdepth 3 -type f ! -name \"*'*\" -print -quit 2>/dev/null"),
    )
    .await?;
    if output.status != 0 {
        return Ok(None);
    }
    let path = output.stdout.trim().to_string();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(path))
}
