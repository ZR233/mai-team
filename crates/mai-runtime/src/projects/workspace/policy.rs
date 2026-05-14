use std::path::{Component, Path};

use crate::{Result, RuntimeError};

pub(crate) struct GitPolicy {
    pub(crate) allowed_remote: String,
    pub(crate) default_branch: String,
    pub(crate) agent_branch_prefix: String,
}

impl Default for GitPolicy {
    fn default() -> Self {
        Self {
            allowed_remote: "origin".to_string(),
            default_branch: "main".to_string(),
            agent_branch_prefix: "mai-agent/".to_string(),
        }
    }
}

impl GitPolicy {
    pub(crate) fn new(default_branch: impl Into<String>) -> Self {
        Self {
            default_branch: default_branch.into(),
            ..Self::default()
        }
    }

    pub(crate) fn validate_remote(&self, remote: &str) -> Result<()> {
        if remote == self.allowed_remote {
            Ok(())
        } else {
            Err(RuntimeError::InvalidInput(format!(
                "unsupported git remote `{remote}`; only `{}` is allowed",
                self.allowed_remote
            )))
        }
    }

    pub(crate) fn validate_branch(&self, branch: &str) -> Result<()> {
        if branch.trim().is_empty()
            || branch.starts_with('/')
            || branch.ends_with('/')
            || branch.starts_with('.')
            || branch.contains("..")
            || branch.contains("//")
            || branch.contains("@{")
            || branch.contains('\\')
            || branch.ends_with(".lock")
            || branch.chars().any(char::is_control)
        {
            return Err(RuntimeError::InvalidInput(format!(
                "unsafe git branch `{branch}`"
            )));
        }
        Ok(())
    }

    pub(crate) fn validate_path(&self, path: &str) -> Result<()> {
        if path.trim().is_empty()
            || path.contains('\\')
            || path.chars().any(char::is_control)
            || Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(RuntimeError::InvalidInput(format!(
                "unsafe git path `{path}`"
            )));
        }
        Ok(())
    }

    pub(crate) fn validate_fetch_refspec(
        &self,
        refspec: Option<&str>,
        default_branch: &str,
    ) -> Result<()> {
        let Some(refspec) = refspec else {
            return Ok(());
        };
        if refspec == default_branch
            || refspec == format!("refs/heads/{default_branch}")
            || is_pull_request_head_ref(refspec)
        {
            Ok(())
        } else {
            Err(RuntimeError::InvalidInput(format!(
                "unsupported git fetch refspec `{refspec}`"
            )))
        }
    }
}

fn is_pull_request_head_ref(refspec: &str) -> bool {
    let refspec = refspec.strip_prefix("refs/").unwrap_or(refspec);
    let Some(rest) = refspec.strip_prefix("pull/") else {
        return false;
    };
    let Some(number) = rest.strip_suffix("/head") else {
        return false;
    };
    !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_non_origin_remotes() {
        let policy = GitPolicy::default();

        assert!(policy.validate_remote("origin").is_ok());
        assert!(policy.validate_remote("upstream").is_err());
        assert!(
            policy
                .validate_remote("https://example.com/repo.git")
                .is_err()
        );
    }

    #[test]
    fn policy_rejects_unsafe_branch_names() {
        let policy = GitPolicy::default();

        assert!(policy.validate_branch("feature/safe-name").is_ok());
        assert!(policy.validate_branch("").is_err());
        assert!(policy.validate_branch("../escape").is_err());
        assert!(policy.validate_branch("/absolute").is_err());
        assert!(policy.validate_branch("bad\\branch").is_err());
        assert!(policy.validate_branch("bad\nbranch").is_err());
    }

    #[test]
    fn policy_rejects_unsafe_paths() {
        let policy = GitPolicy::default();

        assert!(policy.validate_path("src/lib.rs").is_ok());
        assert!(policy.validate_path("../secret").is_err());
        assert!(policy.validate_path("/etc/passwd").is_err());
        assert!(policy.validate_path("bad\\path").is_err());
        assert!(policy.validate_path("bad\u{7f}path").is_err());
    }

    #[test]
    fn policy_allows_default_and_pr_fetch_refspecs_only() {
        let policy = GitPolicy::default();

        assert!(policy.validate_fetch_refspec(None, "main").is_ok());
        assert!(policy.validate_fetch_refspec(Some("main"), "main").is_ok());
        assert!(
            policy
                .validate_fetch_refspec(Some("pull/42/head"), "main")
                .is_ok()
        );
        assert!(
            policy
                .validate_fetch_refspec(Some("+refs/heads/main:refs/heads/main"), "main")
                .is_err()
        );
        assert!(
            policy
                .validate_fetch_refspec(Some("refs/tags/v1.0.0"), "main")
                .is_err()
        );
    }
}
