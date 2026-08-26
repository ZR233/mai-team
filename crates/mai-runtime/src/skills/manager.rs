use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mai_protocol::{SkillMetadata, SkillScope, SkillsConfigRequest, SkillsListResponse};
use pl_core::skill::{
    FileSystemSkillProvider, FrozenSkillCatalog, SkillDirectorySource, SkillProviderRequest,
    SkillRegistry, SkillResourceBase, SkillSourceKind,
};
use tokio_util::sync::CancellationToken;

use crate::config::MaiSkillsConfig;
use crate::{Result, RuntimeError};

/// mai 产品层声明 Skill 来源，PL 负责发现、校验、冻结、加载与资源读取。
#[derive(Debug, Clone)]
pub struct SkillCatalogService {
    workspace_root: PathBuf,
    sources: Vec<SkillDirectorySource>,
}

impl SkillCatalogService {
    pub fn new_with_system_root(
        repo_root: impl AsRef<Path>,
        system_root: Option<impl AsRef<Path>>,
    ) -> Self {
        let repo_root = repo_root.as_ref().to_path_buf();
        let mut sources = vec![SkillDirectorySource::new(
            repo_root.join(".agents/skills"),
            SkillSourceKind::External,
        )];
        if let Some(home) = dirs::home_dir() {
            sources.push(SkillDirectorySource::new(
                home.join(".agents/skills"),
                SkillSourceKind::User,
            ));
        }
        if let Some(system_root) = system_root {
            sources.push(SkillDirectorySource::new(
                system_root.as_ref(),
                SkillSourceKind::System,
            ));
        }
        Self {
            workspace_root: repo_root,
            sources,
        }
    }

    pub fn with_roots(
        workspace_root: impl Into<PathBuf>,
        roots: Vec<(PathBuf, SkillScope)>,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            sources: skill_sources(roots),
        }
    }

    pub fn clone_with_extra_roots(&self, roots: Vec<(PathBuf, SkillScope)>) -> Self {
        let mut sources = skill_sources(roots);
        sources.extend(self.sources.clone());
        Self {
            workspace_root: self.workspace_root.clone(),
            sources,
        }
    }

    /// 发现执行期唯一冻结目录。被禁用名称不会进入该目录。
    pub async fn discover(
        &self,
        request: &SkillsConfigRequest,
        policy: &MaiSkillsConfig,
        cancellation: CancellationToken,
    ) -> Result<Arc<FrozenSkillCatalog>> {
        self.discover_with_disabled(effective_disabled(request, policy), cancellation)
            .await
    }

    /// 为设置与项目页面发现完整目录，再投影产品启用状态。
    pub async fn list(
        &self,
        request: &SkillsConfigRequest,
        policy: &MaiSkillsConfig,
    ) -> Result<SkillsListResponse> {
        let catalog = self
            .discover_with_disabled(Vec::new(), CancellationToken::new())
            .await?;
        Ok(self.project(&catalog, request, policy))
    }

    pub fn project(
        &self,
        catalog: &FrozenSkillCatalog,
        request: &SkillsConfigRequest,
        policy: &MaiSkillsConfig,
    ) -> SkillsListResponse {
        let disabled = effective_disabled(request, policy)
            .into_iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        let enabled = policy.enabled;
        let skills = catalog
            .snapshot()
            .skills
            .iter()
            .map(|skill| SkillMetadata {
                name: skill.name.clone(),
                description: skill.description.clone(),
                short_description: None,
                path: skill_document_path(&skill.resource_base),
                source_path: None,
                scope: product_scope(skill.source),
                enabled: enabled && !disabled.contains(&skill.name.to_ascii_lowercase()),
                interface: None,
                dependencies: None,
                policy: None,
            })
            .collect();
        let errors = catalog
            .snapshot()
            .warnings
            .iter()
            .map(|message| mai_protocol::SkillErrorInfo {
                path: PathBuf::new(),
                message: message.clone(),
            })
            .collect();
        SkillsListResponse {
            roots: self
                .sources
                .iter()
                .map(|source| source.root.clone())
                .collect(),
            skills,
            errors,
        }
    }

    async fn discover_with_disabled(
        &self,
        disabled: Vec<String>,
        cancellation: CancellationToken,
    ) -> Result<Arc<FrozenSkillCatalog>> {
        let provider = FileSystemSkillProvider::from_directories(
            "mai-filesystem-skills",
            self.sources.clone(),
        )
        .map_err(RuntimeError::Model)?;
        let registry = SkillRegistry::new();
        let _registration = registry
            .register(Arc::new(provider))
            .map_err(RuntimeError::Model)?;
        let config = pl_core::SkillsConfig {
            auto_learn: false,
            disabled,
            ..Default::default()
        };
        registry
            .discover(SkillProviderRequest {
                workspace_root: self.workspace_root.clone(),
                config,
                system_dir: None,
                cancellation,
            })
            .await
            .map(Arc::new)
            .map_err(RuntimeError::Model)
    }
}

pub fn normalize_config(config: &SkillsConfigRequest) -> Result<SkillsConfigRequest> {
    let mut disabled = BTreeSet::new();
    for name in &config.disabled {
        let name = name.trim();
        pl_core::skill::validate_skill_name(name).map_err(RuntimeError::Model)?;
        disabled.insert(name.to_string());
    }
    Ok(SkillsConfigRequest {
        disabled: disabled.into_iter().collect(),
    })
}

fn effective_disabled(request: &SkillsConfigRequest, policy: &MaiSkillsConfig) -> Vec<String> {
    policy
        .disabled
        .iter()
        .chain(&request.disabled)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn skill_sources(roots: Vec<(PathBuf, SkillScope)>) -> Vec<SkillDirectorySource> {
    roots
        .into_iter()
        .map(|(root, scope)| SkillDirectorySource::new(root, framework_scope(scope)))
        .collect()
}

fn framework_scope(scope: SkillScope) -> SkillSourceKind {
    match scope {
        SkillScope::Project => SkillSourceKind::Project,
        SkillScope::Repo => SkillSourceKind::External,
        SkillScope::User => SkillSourceKind::User,
        SkillScope::System => SkillSourceKind::System,
    }
}

fn product_scope(scope: SkillSourceKind) -> SkillScope {
    match scope {
        SkillSourceKind::Project => SkillScope::Project,
        SkillSourceKind::User => SkillScope::User,
        SkillSourceKind::System => SkillScope::System,
        SkillSourceKind::External => SkillScope::Repo,
    }
}

fn skill_document_path(base: &SkillResourceBase) -> PathBuf {
    match base {
        SkillResourceBase::Directory { path } => path.join(pl_core::skill::SKILL_FILE_NAME),
        SkillResourceBase::Url { .. } | SkillResourceBase::Opaque { .. } => PathBuf::new(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn pl_catalog_resolves_duplicate_names_and_applies_product_disable_policy() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let system = root.path().join("system");
        write_skill(&project, "review", "project");
        write_skill(&system, "review", "system");
        let service = SkillCatalogService::with_roots(
            root.path(),
            vec![(project, SkillScope::Project), (system, SkillScope::System)],
        );
        let request = SkillsConfigRequest {
            disabled: vec!["review".to_string()],
        };
        let policy = MaiSkillsConfig::default();

        let execution = service
            .discover(&request, &policy, CancellationToken::new())
            .await
            .unwrap();
        let listed = service.list(&request, &policy).await.unwrap();

        assert!(execution.snapshot().skills.is_empty());
        assert_eq!(listed.skills.len(), 1);
        assert_eq!(listed.skills[0].description, "project");
        assert!(!listed.skills[0].enabled);
    }

    fn write_skill(root: &Path, name: &str, description: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(pl_core::skill::SKILL_FILE_NAME),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }
}
