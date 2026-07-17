/// 项目默认分支 working tree 对应的精确版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRepositoryRevision {
    pub(crate) branch: String,
    pub(crate) base_sha: String,
}

/// 同步项目仓库时需要一并固定的 PR ref。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRepositoryReviewTarget {
    pub(crate) pr: u64,
    pub(crate) head_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectRepositorySyncTarget {
    DefaultBranch,
    Review(ProjectRepositoryReviewTarget),
}

impl ProjectRepositorySyncTarget {
    pub(crate) fn review(&self) -> Option<&ProjectRepositoryReviewTarget> {
        match self {
            Self::DefaultBranch => None,
            Self::Review(target) => Some(target),
        }
    }
}

const BASE_SHA_MARKER: &str = "MAI_PROJECT_BASE_SHA=";
const HEAD_SHA_MARKER: &str = "MAI_PROJECT_HEAD_SHA=";

pub(crate) fn sync_result_markers() -> (&'static str, &'static str) {
    (BASE_SHA_MARKER, HEAD_SHA_MARKER)
}

pub(crate) fn parse_sync_result(
    branch: &str,
    stdout: &str,
    expected_head: Option<&str>,
) -> crate::Result<ProjectRepositoryRevision> {
    let base_sha = marker_value(stdout, BASE_SHA_MARKER).ok_or_else(|| {
        crate::RuntimeError::InvalidInput(
            "project repository sync did not report the default branch SHA".to_string(),
        )
    })?;
    if let Some(expected_head) = expected_head {
        let actual_head = marker_value(stdout, HEAD_SHA_MARKER).ok_or_else(|| {
            crate::RuntimeError::InvalidInput(
                "project repository sync did not report the pull request head SHA".to_string(),
            )
        })?;
        if actual_head != expected_head {
            return Err(crate::RuntimeError::InvalidInput(format!(
                "project repository pull request head mismatch: expected {expected_head}, fetched {actual_head}"
            )));
        }
    }
    Ok(ProjectRepositoryRevision {
        branch: branch.to_string(),
        base_sha: base_sha.to_string(),
    })
}

fn marker_value<'a>(stdout: &'a str, marker: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(marker))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_exact_repository_revisions() {
        let revision = parse_sync_result(
            "main",
            "fetch output\nMAI_PROJECT_BASE_SHA=base123\nMAI_PROJECT_HEAD_SHA=head456\n",
            Some("head456"),
        )
        .expect("parse sync output");

        assert_eq!(
            revision,
            ProjectRepositoryRevision {
                branch: "main".to_string(),
                base_sha: "base123".to_string(),
            }
        );
    }

    #[test]
    fn rejects_fetched_head_that_differs_from_github() {
        let error = parse_sync_result(
            "main",
            "MAI_PROJECT_BASE_SHA=base123\nMAI_PROJECT_HEAD_SHA=stale\n",
            Some("current"),
        )
        .expect_err("reject stale ref");

        assert!(
            error
                .to_string()
                .contains("expected current, fetched stale")
        );
    }
}
