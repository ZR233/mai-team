//! T27 的环境证据输出。

use mai_docker::ExistingEnvironment;
use serde_json::json;

use crate::evidence::Evidence;

use super::environment::BindingEnvironment;

/// 输出绑定、真实容器与 DockerClient 核验结果。
pub(super) fn record_environment(
    evidence: &mut Evidence,
    key: &str,
    environment: &BindingEnvironment,
) {
    evidence.fact(
        key,
        json!({
            "container": &environment.facts,
            "binding": {
                "container_id": &environment.binding.container_id,
                "generation": &environment.binding.generation,
                "effective_image_id": &environment.binding.effective_image_id,
                "applied_revision": &environment.binding.applied_revision,
            },
            "inspect_environment": {
                "matching": matches!(environment.inspected, ExistingEnvironment::Matching(_)),
                "diff": environment.inspected.diff().map(|diff| format!("{diff:?}")),
            },
        }),
    );
}
