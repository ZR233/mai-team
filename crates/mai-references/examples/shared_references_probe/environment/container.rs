//! 容器内命令执行与文本读取的薄封装（统一超时与单引号安全引用）。

use super::*;

/// 在容器里跑一条命令（统一超时，避免悬死）。
pub async fn exec(probe: &Probe, container_id: &str, command: &str) -> Result<ExecOutput> {
    Ok(probe
        .docker
        .exec_shell(container_id, command, Some("/"), Some(120))
        .await?)
}

/// 读容器内文本文件；文件不存在或不可读时返回 `None`。
pub async fn read_text_file(
    probe: &Probe,
    container_id: &str,
    path: &str,
) -> Result<Option<String>> {
    let quoted = shell_quote(path);
    let output = exec(
        probe,
        container_id,
        &format!("test -f {quoted} && cat {quoted}"),
    )
    .await?;
    if output.status != 0 {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

/// 单引号安全引用，供少数需要拼 shell 的验收命令使用。
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
