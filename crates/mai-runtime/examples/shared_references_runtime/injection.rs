//! T38：把带指令文本的参考源注入真实 Runtime，并验证它只作为资料进入模型上下文。
//!
//! 场景边界：
//! - 先由 root 运行 `scripts/references/prepare-injection.py` 创建固定隔离源码；
//! - 本模块只用真实 `ReferenceCatalog` 登记与异步验证，不伪造 Ready/批准收据；
//! - B 项目通过产品 CAS/应用入口绑定该源，等物理环境真实 applied 后再触发真实 turn；
//! - 首次 provider 请求只能看到只读参考清单，不能自动读到 `AGENTS.md`；
//! - 指令文件只有显式 `read_file` 后才可见；
//! - 结束时恢复 B 的原始绑定，但保留未绑定的恶意显示名源给后续 UI 验收。

#[path = "injection/project.rs"]
mod project;
#[path = "injection/provider_evidence.rs"]
mod provider_evidence;
#[path = "injection/source.rs"]
mod source;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::host;
use crate::runtime_config::DriverConfig;

/// `injection` 命令入口。
pub async fn run(config: &DriverConfig, evidence: &mut Evidence) -> Result<()> {
    let store = host::open_store(config).await?;
    let runtime_host = host::start(config, store).await?;
    evidence.fact("provider", runtime_host.configure_models().await?);

    let outcome = scenario(&runtime_host, evidence).await;
    runtime_host.shutdown().await;
    outcome
}

async fn scenario(host: &host::RuntimeHost, evidence: &mut Evidence) -> Result<()> {
    source::ensure_fixture(host, evidence)?;
    let source = source::register_or_reuse(host, evidence).await?;
    project::scenario(host, &source, evidence).await
}
