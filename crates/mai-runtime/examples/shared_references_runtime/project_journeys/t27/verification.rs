//! 换代后的真实 exec 数据保留验证。

use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::host::RuntimeHost;
use crate::probe;

use super::super::support::{self, ProjectHandles};
use super::assertions::checked;
use super::probes;

/// 通过已注册 exec 工具读取三类标记与本地 deb 安装事实。
pub(super) async fn retained_data(
    host: &RuntimeHost,
    handles: &ProjectHandles,
    tag: &str,
    evidence: &mut Evidence,
) -> Result<bool> {
    let retained = probe::run(
        host,
        handles.maintainer,
        &probes::read_data(tag),
        host.config.turn_timeout,
    )
    .await?;
    evidence.fact("t27.retention_probe", json!(&retained));
    support::assert_calls(
        evidence,
        "t27.retention_tool_succeeded",
        &retained,
        &[("exec(读取三类标记)", true)],
    );
    let retained_output = retained
        .call(0)
        .map(|call| call.output.clone())
        .unwrap_or_default();
    let mut valid = checked(
        evidence,
        "t27.three_data_classes_retained",
        retained
            .call(0)
            .is_some_and(|call| call.observed && call.success == Some(true))
            && probes::marker_output_matches(&retained_output, tag),
        format!(
            "三类标记内容完全保留：{}",
            support::first_line(&retained_output)
        ),
        format!("三类标记保留失败或内容不匹配：{retained_output}"),
    );

    let package = probe::run(
        host,
        handles.maintainer,
        &probes::query_package(tag),
        host.config.turn_timeout,
    )
    .await?;
    evidence.fact("t27.package_probe", json!(&package));
    support::assert_calls(
        evidence,
        "t27.dpkg_query_succeeded",
        &package,
        &[("exec(dpkg-query + package payload)", true)],
    );
    let package_output = package
        .call(0)
        .map(|call| call.output.clone())
        .unwrap_or_default();
    valid &= checked(
        evidence,
        "t27.installed_package_retained",
        package
            .call(0)
            .is_some_and(|call| call.observed && call.success == Some(true))
            && probes::package_output_matches(&package_output, tag),
        format!(
            "deb 安装状态与 payload 均保留：{}",
            support::first_line(&package_output)
        ),
        format!("deb 安装数据未保留或内容不匹配：{package_output}"),
    );
    Ok(valid)
}
