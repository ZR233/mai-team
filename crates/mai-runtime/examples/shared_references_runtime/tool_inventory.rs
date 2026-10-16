//! 发现只返回隐藏工具；已公开工具必须从真实 provider 请求的 schema 取事实。
use crate::{
    evidence::Evidence,
    probe::{self, ProbeOutcome},
    runtime_config::DriverConfig,
};
use serde_json::json;
use std::collections::BTreeSet;

pub(super) fn record(config: &DriverConfig, evidence: &mut Evidence, outcome: &ProbeOutcome) {
    let mut names = BTreeSet::new();
    for request in probe::provider_requests(&config.provider_evidence_dir) {
        if !request.to_string().contains(&outcome.probe_id) {
            continue;
        }
        if let Some(tools) = request["tools"].as_array() {
            for tool in tools {
                if let Some(name) = tool["name"]
                    .as_str()
                    .or_else(|| tool["function"]["name"].as_str())
                {
                    names.insert(name.to_owned());
                }
            }
        }
    }
    let discovery = outcome.call(0);
    evidence.fact(
        "tool_inventory_raw",
        json!({ "provider_tools":names, "discovery":discovery }),
    );
    for tool in ["read_file", "list_files", "apply_patch"] {
        let visible = names.contains(tool)
            || discovery
                .is_some_and(|call| call.success == Some(true) && call.output.contains(tool));
        evidence.check(
            &format!("tools.inventory.{tool}"),
            visible,
            format!("真实 provider 工具 schema 或发现结果包含 {tool}"),
            format!("真实 provider 请求未公开 {tool}"),
        );
    }
    evidence.boundary("tools.inventory.backend_reads", "stat_path/read_bytes 是文件后端方法；已注册 read_file 经这两个方法完成真实读取，不另增模型工具别名");
}
