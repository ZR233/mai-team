use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use walkdir::WalkDir;

const SKILL_FILE: &str = "SKILL.md";

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SkillError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSkill {
    pub summary: SkillSummary,
    pub contents: String,
}

#[derive(Debug, Clone)]
pub struct SkillsManager {
    roots: Vec<PathBuf>,
}

impl SkillsManager {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            roots: default_roots(repo_root.as_ref()),
        }
    }

    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn discover(&self) -> Result<Vec<SkillSummary>> {
        let mut skills = Vec::new();
        let mut seen_paths = BTreeSet::new();
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(root).max_depth(6).into_iter().flatten() {
                if !entry.file_type().is_file() || entry.file_name() != SKILL_FILE {
                    continue;
                }
                let path = entry.path().to_path_buf();
                let canonical = fs::canonicalize(&path).unwrap_or(path.clone());
                if !seen_paths.insert(canonical) {
                    continue;
                }
                let contents = fs::read_to_string(&path)?;
                skills.push(parse_skill(&path, &contents));
            }
        }
        skills.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(skills)
    }

    pub fn render_available(&self) -> Result<String> {
        let skills = self.discover()?;
        if skills.is_empty() {
            return Ok("No skills are currently available.".to_string());
        }

        let mut name_counts = BTreeMap::<String, usize>::new();
        for skill in &skills {
            *name_counts.entry(skill.name.clone()).or_default() += 1;
        }

        let lines = skills
            .into_iter()
            .map(|skill| {
                let duplicate = name_counts.get(&skill.name).copied().unwrap_or_default() > 1;
                if duplicate {
                    format!(
                        "- ${}: {} ({})",
                        skill.name,
                        skill.description,
                        skill.path.display()
                    )
                } else {
                    format!("- ${}: {}", skill.name, skill.description)
                }
            })
            .collect::<Vec<_>>();
        Ok(lines.join("\n"))
    }

    pub fn load_explicit(&self, names: &[String]) -> Result<Vec<LoadedSkill>> {
        let requested = names
            .iter()
            .map(|name| name.trim().trim_start_matches('$').to_string())
            .filter(|name| !name.is_empty())
            .collect::<BTreeSet<_>>();
        if requested.is_empty() {
            return Ok(Vec::new());
        }

        let mut loaded = Vec::new();
        for summary in self.discover()? {
            if !requested.contains(&summary.name) {
                continue;
            }
            let contents = fs::read_to_string(&summary.path)?;
            loaded.push(LoadedSkill { summary, contents });
        }
        Ok(loaded)
    }
}

fn default_roots(repo_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".mai-team").join("skills"));
    }
    roots.push(repo_root.join(".agents").join("skills"));
    roots
}

fn parse_skill(path: &Path, contents: &str) -> SkillSummary {
    let dir_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("skill")
        .to_string();
    let mut name = dir_name;
    let mut description = String::new();

    if let Some(frontmatter) = extract_frontmatter(contents) {
        for line in frontmatter.lines() {
            if let Some(value) = line.strip_prefix("name:") {
                name = unquote(value.trim()).to_string();
            } else if let Some(value) = line.strip_prefix("description:") {
                description = unquote(value.trim()).to_string();
            }
        }
    }

    if description.is_empty() {
        description = contents
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("---") && !line.starts_with('#'))
            .unwrap_or("No description provided.")
            .chars()
            .take(240)
            .collect();
    }

    SkillSummary {
        name,
        description,
        path: path.to_path_buf(),
    }
}

fn extract_frontmatter(contents: &str) -> Option<&str> {
    let rest = contents.strip_prefix("---\n")?;
    let (frontmatter, _) = rest.split_once("\n---")?;
    Some(frontmatter)
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_skill_frontmatter() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo-skill\ndescription: Helps demo things.\n---\nbody",
        )
        .expect("write");

        let manager = SkillsManager::with_roots(vec![dir.path().to_path_buf()]);
        let skills = manager.discover().expect("discover");
        assert_eq!(skills[0].name, "demo-skill");
        assert_eq!(skills[0].description, "Helps demo things.");
    }

    #[test]
    fn loads_only_explicit_skills() {
        let dir = tempdir().expect("tempdir");
        for name in ["one", "two"] {
            let skill_dir = dir.path().join(name);
            fs::create_dir_all(&skill_dir).expect("mkdir");
            fs::write(
                skill_dir.join(SKILL_FILE),
                format!("---\nname: {name}\n---\nbody"),
            )
            .expect("write");
        }

        let manager = SkillsManager::with_roots(vec![dir.path().to_path_buf()]);
        let loaded = manager
            .load_explicit(&["two".to_string()])
            .expect("load explicit");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].summary.name, "two");
    }
}
