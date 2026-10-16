//! 目标绑定：别名到绑定的稳定映射、CAS 保存与“仅改说明”轮次。

use super::*;

/// Linux 别名的固定说明。
const DESCRIPTION_LINUX: &str = "参考 IRQ、块设备与驱动生命周期";

/// 私有夹具别名的固定说明。
const DESCRIPTION_PRIVATE: &str = "A 项目私有夹具（B 项目与非项目 Agent 不得访问）";

/// 仅改说明场景追加的后缀；重复运行不会再次追加。
const METADATA_SUFFIX: &str = "［说明更新 1］";

/// 目标绑定集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetBindings {
    LinuxAndFixture,
    MetadataOnly,
    Empty,
}

/// 按别名生成绑定；说明由别名决定，条目 ID 用固定命名空间派生。
fn binding_for(alias: &str, config: &ProbeConfig, source_id: Uuid) -> ProjectReferenceBinding {
    let description = if alias == config.linux_alias {
        DESCRIPTION_LINUX
    } else {
        DESCRIPTION_PRIVATE
    };
    ProjectReferenceBinding {
        id: crate::fixtures::fixture_uuid("binding", alias),
        source_id,
        alias: alias.to_string(),
        description: description.to_string(),
        enabled: true,
    }
}

/// 把 A 项目绑定写成给定别名集合（B 项目始终空绑定）。
pub(super) async fn save_alias_bindings(
    probe: &Probe,
    aliases: &[String],
) -> Result<ProjectReferenceConfiguration> {
    let project = ProjectKind::A.id();
    let current = probe.store.project_reference_configuration(project).await?;
    let mut bindings = Vec::new();
    for alias in aliases {
        let source_id = environment::required(
            probe.sources.by_alias(&probe.config, alias),
            &format!("别名 {alias} 对应的登记源"),
        )?;
        bindings.push(binding_for(alias, &probe.config, source_id));
    }
    let saved = probe
        .applications
        .replace(
            project,
            ReplaceProjectReferencesRequest {
                expected_revision: current.revision,
                bindings,
            },
        )
        .await?;
    let project_b = probe
        .store
        .project_reference_configuration(ProjectKind::B.id())
        .await?;
    if !project_b.bindings.is_empty() {
        probe
            .applications
            .replace(
                ProjectKind::B.id(),
                ReplaceProjectReferencesRequest {
                    expected_revision: project_b.revision,
                    bindings: Vec::new(),
                },
            )
            .await?;
    }
    Ok(saved)
}

/// 保存目标绑定。
pub(super) async fn save_targets(
    probe: &Probe,
    target: TargetBindings,
) -> Result<ProjectReferenceConfiguration> {
    match target {
        TargetBindings::Empty => save_alias_bindings(probe, &[]).await,
        TargetBindings::LinuxAndFixture => {
            let aliases = [
                probe.config.linux_alias.clone(),
                probe.config.private_alias.clone(),
            ];
            save_alias_bindings(probe, &aliases).await
        }
        TargetBindings::MetadataOnly => {
            let project = ProjectKind::A.id();
            let current = probe.store.project_reference_configuration(project).await?;
            if current.bindings.is_empty() {
                return err(
                    "metadata_only_requires_binding",
                    "metadata_only 场景需要先有绑定：请先运行 apply --scenario=bind",
                );
            }
            let mut bindings = current.bindings.clone();
            for binding in bindings.iter_mut() {
                if !binding.description.contains(METADATA_SUFFIX) {
                    binding.description = format!("{}{METADATA_SUFFIX}", binding.description);
                }
            }
            probe
                .applications
                .replace(
                    project,
                    ReplaceProjectReferencesRequest {
                        expected_revision: current.revision,
                        bindings,
                    },
                )
                .await
                .map_err(Into::into)
        }
    }
}

/// seed 阶段写入的项目绑定（缺省为空：先有干净环境，再由 apply 做真实换代）。
pub(super) async fn write_seed_bindings(probe: &Probe, report: &mut Report) -> Result<()> {
    let aliases = probe.config.seed_bindings.clone();
    let saved = save_alias_bindings(probe, &aliases).await?;
    report.fact(
        "seed_bindings",
        json!({ "aliases": aliases, "revision": saved.revision, "bindings": saved.bindings }),
    )?;
    Ok(())
}
