use std::path::{Path, PathBuf};

use mai_protocol::{SkillScope, SkillsConfigRequest, SkillsListResponse};

use super::config::apply_config;
use super::error::Result;
#[cfg(test)]
use super::injection::SkillSelection;
use super::injection::{SkillInjections, SkillInput, build_injections_from_outcome};
#[cfg(test)]
use super::render::render_available_response;
use super::scan::{
    SkillLoadOutcome, SkillRoot, default_roots_with_system, roots_from_pairs, scan_roots,
};

#[derive(Debug, Clone)]
pub struct SkillsManager {
    roots: Vec<SkillRoot>,
}

impl SkillsManager {
    pub fn new_with_system_root(
        repo_root: impl AsRef<Path>,
        system_root: Option<impl AsRef<Path>>,
    ) -> Self {
        Self {
            roots: default_roots_with_system(
                repo_root.as_ref(),
                system_root.as_ref().map(|path| path.as_ref()),
            ),
        }
    }

    #[cfg(test)]
    pub fn new_with_system_root_and_extra_roots(
        repo_root: impl AsRef<Path>,
        system_root: Option<impl AsRef<Path>>,
        extra_roots: Vec<(PathBuf, SkillScope)>,
    ) -> Self {
        let mut roots = default_roots_with_system(
            repo_root.as_ref(),
            system_root.as_ref().map(|path| path.as_ref()),
        );
        roots.extend(roots_from_pairs(extra_roots));
        Self { roots }
    }

    pub fn with_roots(roots: Vec<(PathBuf, SkillScope)>) -> Self {
        Self {
            roots: roots_from_pairs(roots),
        }
    }

    pub fn clone_with_extra_roots(&self, extra_roots: Vec<(PathBuf, SkillScope)>) -> Self {
        let mut roots = self.roots.clone();
        roots.extend(roots_from_pairs(extra_roots));
        Self { roots }
    }

    pub fn list(&self, config: &SkillsConfigRequest) -> Result<SkillsListResponse> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(SkillsListResponse {
            roots: outcome.roots,
            skills: outcome.skills,
            errors: outcome.errors,
        })
    }

    #[cfg(test)]
    pub fn render_available(&self, config: &SkillsConfigRequest) -> Result<String> {
        Ok(render_available_response(self.list(config)?))
    }

    #[cfg(test)]
    pub fn build_injections(
        &self,
        explicit_mentions: &[String],
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_from_outcome(
            &outcome,
            &SkillInput {
                selections: explicit_mentions
                    .iter()
                    .map(|mention| SkillSelection::from_mention(mention.clone()))
                    .collect(),
                ..Default::default()
            },
        ))
    }

    #[cfg(test)]
    pub fn build_injections_for_message(
        &self,
        message: &str,
        explicit_mentions: &[String],
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_from_outcome(
            &outcome,
            &SkillInput {
                text: Some(message),
                selections: explicit_mentions
                    .iter()
                    .map(|mention| SkillSelection::from_mention(mention.clone()))
                    .collect(),
                ..Default::default()
            },
        ))
    }

    pub fn build_injections_for_input(
        &self,
        input: SkillInput<'_>,
        config: &SkillsConfigRequest,
    ) -> Result<SkillInjections> {
        let mut outcome = self.load_outcome();
        apply_config(&mut outcome.skills, config)?;
        Ok(build_injections_from_outcome(&outcome, &input))
    }

    fn load_outcome(&self) -> SkillLoadOutcome {
        scan_roots(&self.roots)
    }
}
