//! 文件通道用例：`Runtime::upload_file` / `download_file_tar` 经真实租约 + Docker cp。
//!
//! 判定要点：
//! - 读参考源成功（下载 `/references/<alias>/README.md` 的内容里能读到夹具文本）；
//! - 写参考源被拒（直接目标、以及经工作区内符号链接解析后的目标），拒绝原因必须是
//!   `read-only reference namespace`（产品写策略在 Docker 之前先拒），且宿主原件摘要不变；
//! - 普通工作区上传/下载往返成功（正向对照，证明拒绝来自授权而不是通道坏了）。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use mai_protocol::AgentId;
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::host::{self, RuntimeHost};
use crate::probe::{self, Probe, ProbeCall};
use crate::runtime_config::DriverConfig;

use super::outputs;

/// 工作区根（与夹具标记 `/workspace/repo/acceptance-marker.txt` 一致）。
const WORKSPACE_ROOT: &str = "/workspace/repo";

/// 夹具 README 里必然出现的文本（前一领域驱动写入的夹具源码）。
const FIXTURE_README_MARKER: &str = "A 项目私有参考夹具";

/// 执行文件通道用例。
pub async fn case(
    config: &DriverConfig,
    host: &RuntimeHost,
    evidence: &mut Evidence,
    tag: &str,
    agent: AgentId,
    private_target: &str,
) -> Result<()> {
    let digest_before = host::host_source_digest(config, &config.fixture_repo);
    evidence.fact(
        "transfer.digest_before",
        json!({ "fixture_digest": digest_before }),
    );

    // 1) 下载参考源必须成功（只读 bind 里 Docker cp 可读）。
    let readme_path = format!("{private_target}/README.md");
    match host
        .runtime
        .download_file_tar(agent, readme_path.clone())
        .await
    {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let hit = text.contains(FIXTURE_README_MARKER);
            evidence.fact(
                "transfer.download_reference",
                json!({ "path": readme_path, "bytes": bytes.len(), "contains_marker": hit }),
            );
            evidence.check(
                "channels.transfer.download_reference_ok",
                hit,
                format!(
                    "download_file_tar 读到 {}（{} 字节，含夹具标记）",
                    readme_path,
                    bytes.len()
                ),
                format!(
                    "download_file_tar 返回了 {} 字节但缺少夹具标记（前 200 字节：{:?}）",
                    bytes.len(),
                    text.chars().take(200).collect::<String>()
                ),
            );
        }
        Err(cause) => evidence.fail(
            "channels.transfer.download_reference_ok",
            format!("download_file_tar({readme_path}) 失败：{cause}"),
        ),
    }

    // 2) 直接上传到 /references 必须被拒。
    let denied_path = format!("{private_target}/runtime-channel-upload-{tag}.txt");
    let payload = format!("channel upload attempt {tag}");
    let encoded = BASE64.encode(payload.as_bytes());
    match host
        .runtime
        .upload_file(agent, denied_path.clone(), encoded.clone())
        .await
    {
        Ok(bytes) => evidence.fail(
            "channels.transfer.upload_reference_denied",
            format!("上传到参考源竟然成功（{bytes} 字节写入 {denied_path}）"),
        ),
        Err(cause) => {
            let message = cause.to_string();
            evidence.fact(
                "transfer.upload_reference_error",
                json!({ "path": denied_path, "error": message }),
            );
            evidence.check(
                "channels.transfer.upload_reference_denied",
                message.contains("read-only reference namespace"),
                format!("上传到参考源被拒：{}", outputs::first_line(&message)),
                format!(
                    "上传到参考源失败但原因不是拒写参考命名空间：{}",
                    outputs::first_line(&message)
                ),
            );
        }
    }
    assert_upload_absent(host, evidence, agent, &denied_path).await?;

    // 3) 经工作区内符号链接上传到参考源同样必须被拒（容器内 readlink 解析后仍拒）。
    let link = format!("{WORKSPACE_ROOT}/runtime-channel-link-{tag}");
    let link_probe = Probe::new(
        format!("channels-link-{tag}"),
        vec![ProbeCall::new(
            "exec",
            json!({ "command": format!("ln -sfn {private_target} {link} && echo linked=1") }),
        )],
    );
    let link_outcome = probe::run(host, agent, &link_probe, config.turn_timeout).await?;
    evidence.fact("transfer.symlink_probe", json!(&link_outcome));
    let link_ready = link_outcome
        .call(0)
        .is_some_and(|call| call.observed && call.success == Some(true));
    if !link_ready {
        evidence.blocked(
            "channels.transfer.upload_symlink_denied",
            "无法在工作区建立指向参考源的符号链接（exec 未成功），该子用例跳过",
        );
    } else {
        let via_link = format!("{link}/runtime-channel-upload-{tag}.txt");
        match host
            .runtime
            .upload_file(agent, via_link.clone(), encoded)
            .await
        {
            Ok(bytes) => evidence.fail(
                "channels.transfer.upload_symlink_denied",
                format!("经符号链接上传竟然成功（{bytes} 字节写入 {via_link}）"),
            ),
            Err(cause) => {
                let message = cause.to_string();
                evidence.fact(
                    "transfer.upload_symlink_error",
                    json!({ "path": via_link, "error": message }),
                );
                evidence.check(
                    "channels.transfer.upload_symlink_denied",
                    message.contains("read-only reference namespace")
                        && message.contains(private_target),
                    format!(
                        "经符号链接上传被拒且原因指向解析后的参考路径：{}",
                        outputs::first_line(&message)
                    ),
                    format!(
                        "经符号链接上传的拒绝原因不含解析后的参考路径：{}",
                        outputs::first_line(&message)
                    ),
                );
            }
        }
        assert_upload_absent(
            host,
            evidence,
            agent,
            &format!("{private_target}/runtime-channel-upload-{tag}.txt"),
        )
        .await?;
    }

    // 4) 普通工作区上传/下载往返必须成功（正向对照）。
    let workspace_path = format!("{WORKSPACE_ROOT}/runtime-channel-{tag}.txt");
    let content = format!("workspace channel payload {tag}");
    let encoded = BASE64.encode(content.as_bytes());
    match host
        .runtime
        .upload_file(agent, workspace_path.clone(), encoded)
        .await
    {
        Ok(bytes) => {
            evidence.check(
                "channels.transfer.upload_workspace_ok",
                bytes == content.len(),
                format!("工作区上传写入 {bytes} 字节到 {workspace_path}"),
                format!("工作区上传返回 {bytes} 字节，期望 {}", content.len()),
            );
            match host
                .runtime
                .download_file_tar(agent, workspace_path.clone())
                .await
            {
                Ok(downloaded) => {
                    let text = String::from_utf8_lossy(&downloaded).into_owned();
                    evidence.check(
                        "channels.transfer.download_workspace_ok",
                        text.contains(&content),
                        format!(
                            "工作区下载往返成功（{} 字节，含上传内容）",
                            downloaded.len()
                        ),
                        format!("工作区下载内容不含上传内容（{} 字节）", downloaded.len()),
                    );
                }
                Err(cause) => evidence.fail(
                    "channels.transfer.download_workspace_ok",
                    format!("下载 {workspace_path} 失败：{cause}"),
                ),
            }
        }
        Err(cause) => evidence.fail(
            "channels.transfer.upload_workspace_ok",
            format!("上传 {workspace_path} 失败（工作区应可写）：{cause}"),
        ),
    }

    // 5) 宿主原件摘要必须不变。
    let digest_after = host::host_source_digest(config, &config.fixture_repo);
    evidence.fact(
        "transfer.digest_after",
        json!({ "fixture_digest": digest_after }),
    );
    match (&digest_before, &digest_after) {
        (Some(before), Some(after)) => evidence.check(
            "channels.transfer.host_digest_unchanged",
            before == after,
            format!("夹具源宿主摘要前后一致（{after}）"),
            format!("夹具源宿主摘要发生变化：{before} -> {after}"),
        ),
        _ => evidence.blocked(
            "channels.transfer.host_digest_unchanged",
            "本轮没有采到夹具源的宿主摘要（部署策略/信任模式不允许扫描）",
        ),
    }
    Ok(())
}

/// 被拒绝的写入不得真的产生文件：下载该路径必须失败。
async fn assert_upload_absent(
    host: &RuntimeHost,
    evidence: &mut Evidence,
    agent: AgentId,
    path: &str,
) -> Result<()> {
    match host
        .runtime
        .download_file_tar(agent, path.to_string())
        .await
    {
        Ok(bytes) => evidence.fail(
            format!("channels.transfer.absent.{}", last_component(path)),
            format!("被拒绝的上传竟然产生了文件 {path}（{} 字节）", bytes.len()),
        ),
        Err(cause) => evidence.pass(
            format!("channels.transfer.absent.{}", last_component(path)),
            format!(
                "{path} 不存在，写入确实被拒（下载失败：{}）",
                outputs::first_line(&cause.to_string())
            ),
        ),
    }
    Ok(())
}

fn last_component(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("path").to_string()
}
