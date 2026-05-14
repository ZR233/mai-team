use mai_protocol::{SkillConfigEntry, SkillMetadata, SkillScope, SkillsConfigRequest};

use crate::error::{Result, SkillError};
use crate::paths::canonicalize_or_clone;

pub fn normalize_config(config: &SkillsConfigRequest) -> Result<SkillsConfigRequest> {
    let mut out = SkillsConfigRequest::default();
    for entry in &config.config {
        match (entry.path.as_ref(), entry.name.as_deref()) {
            (Some(path), None) => out.config.push(SkillConfigEntry {
                path: Some(canonicalize_or_clone(path)),
                name: None,
                enabled: entry.enabled,
            }),
            (None, Some(name)) => {
                let name = name.trim();
                if name.is_empty() {
                    return Err(SkillError::InvalidConfig(
                        "skill config name cannot be empty".to_string(),
                    ));
                }
                out.config.push(SkillConfigEntry {
                    name: Some(name.to_string()),
                    path: None,
                    enabled: entry.enabled,
                });
            }
            (Some(_), Some(_)) => {
                return Err(SkillError::InvalidConfig(
                    "skill config entries must use either path or name, not both".to_string(),
                ));
            }
            (None, None) => {
                return Err(SkillError::InvalidConfig(
                    "skill config entries must include path or name".to_string(),
                ));
            }
        }
    }
    Ok(out)
}

pub(crate) fn apply_config(
    skills: &mut [SkillMetadata],
    config: &SkillsConfigRequest,
) -> Result<()> {
    let normalized = normalize_config(config)?;
    for entry in normalized.config {
        match (entry.path, entry.name) {
            (Some(path), None) => {
                for skill in skills
                    .iter_mut()
                    .filter(|skill| skill.scope != SkillScope::Project && skill.path == path)
                {
                    skill.enabled = entry.enabled;
                }
            }
            (None, Some(name)) => {
                for skill in skills
                    .iter_mut()
                    .filter(|skill| skill.scope != SkillScope::Project && skill.name == name)
                {
                    skill.enabled = entry.enabled;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
