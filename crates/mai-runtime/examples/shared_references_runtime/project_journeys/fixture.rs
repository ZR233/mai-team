//! 项目夹具清单：本地 Git Smart HTTP + GitHub API 的只读契约。
//!
//! 夹具由 `scripts/references/project-fixture.py` 启动，启动时写出
//! `<root>/fixture/github-fixture.json`。驱动只读该清单：BASE/PR head 的 commit SHA、
//! 仓库全名、假安装标识、以及两个 BASE/HEAD 标记文件的内容，全部来自真实 `git` 提交，
//! 不由驱动猜测。清单缺失或非法一律记 `blocked`（绝不退回"猜一个 SHA"）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::Result;
use crate::evidence::Evidence;
use crate::runtime_config::DriverConfig;

/// 夹具 GitHub API / Git Smart HTTP 的缺省地址（与夹具脚本一致）。
pub const DEFAULT_API_BASE_URL: &str = "http://127.0.0.1:18082";

/// 夹具项目仓库全名：Git 传输靠 sidecar 镜像的 `url.<base>.insteadOf` 重写到本地夹具。
pub const REPOSITORY_FULL_NAME: &str = "mai-reference-fixture/reference-main";

/// reviewer 只读仓库视图的容器路径（与产品 `PROJECT_REPOSITORY_CONTAINER_PATH` 一致）。
pub const REVIEW_CONTAINER_PATH: &str = "/project/repo";

/// review 快照在项目缓存卷内的相对子路径前缀（与产品 `PROJECT_REVIEW_SNAPSHOT_ROOT` 一致）。
pub const REVIEW_SNAPSHOT_ROOT: &str = "review-contexts";

/// 夹具清单路径的环境变量覆盖（root 可显式指定）。
pub const MANIFEST_ENV: &str = "MAI_ACCEPTANCE_GITHUB_FIXTURE";

/// 夹具清单缺省位置：`<root>/fixture/github-fixture.json`。
pub fn manifest_path(config: &DriverConfig) -> PathBuf {
    std::env::var(MANIFEST_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| config.root.join("fixture").join("github-fixture.json"))
}

/// review 快照在缓存卷内的相对子路径：`review-contexts/<run_id>/repo`。
pub fn review_snapshot_subpath(run_id: &str) -> String {
    format!("{REVIEW_SNAPSHOT_ROOT}/{run_id}/repo")
}

/// 夹具清单（只读契约）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubFixture {
    #[serde(default = "default_api_base_url")]
    pub github_api_base_url: String,
    pub repository_full_name: String,
    pub owner: String,
    pub name: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    pub pull_request: u64,
    pub base_sha: String,
    pub head_sha: String,
    pub installation_id: u64,
    pub installation_account: String,
    #[serde(default = "default_app_id")]
    pub app_id: String,
    #[serde(default = "default_bot_login")]
    pub app_bot_login: String,
    #[serde(default = "default_bot_user_id")]
    pub app_bot_user_id: u64,
    #[serde(default = "default_fake_token")]
    pub fake_token: String,
    pub base_label_path: String,
    pub base_label: String,
    pub head_label_path: String,
    pub head_label: String,
}

fn default_api_base_url() -> String {
    DEFAULT_API_BASE_URL.to_string()
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_app_id() -> String {
    "fixture-app-1".to_string()
}

fn default_bot_login() -> String {
    "mai-reference-fixture[bot]".to_string()
}

fn default_bot_user_id() -> u64 {
    900001
}

fn default_fake_token() -> String {
    "fixture-installation-token".to_string()
}

impl GitHubFixture {
    /// 读取夹具清单；缺失/非法时记录 `blocked` 并返回 `None`。
    pub fn load(config: &DriverConfig, evidence: &mut Evidence) -> Result<Option<Self>> {
        let path = manifest_path(config);
        let text = match std::fs::read(&path) {
            Ok(text) => text,
            Err(cause) => {
                evidence.blocked(
                    "projects.fixture_manifest",
                    format!(
                        "缺少夹具清单 {}（{cause}）；root 需要先启动 `python3 scripts/references/project-fixture.py --root {}`，\
                         它会同时提供 Git Smart HTTP 与 GitHub API 并写出该清单",
                        path.display(),
                        config.root.display()
                    ),
                );
                return Ok(None);
            }
        };
        let fixture: Self = match serde_json::from_slice(&text) {
            Ok(fixture) => fixture,
            Err(cause) => {
                evidence.blocked(
                    "projects.fixture_manifest",
                    format!("夹具清单 {} 不是合法 JSON 契约：{cause}", path.display()),
                );
                return Ok(None);
            }
        };
        evidence.fact(
            "projects.fixture_manifest_path",
            json!(path.display().to_string()),
        );
        evidence.fact("projects.fixture", json!(&fixture));
        if fixture.repository_full_name != REPOSITORY_FULL_NAME {
            evidence.blocked(
                "projects.fixture_repository",
                format!(
                    "夹具仓库全名是 `{}`，验收要求 `{REPOSITORY_FULL_NAME}`（sidecar 的 insteadOf 前缀只覆盖后者）",
                    fixture.repository_full_name
                ),
            );
            return Ok(None);
        }
        if fixture.base_sha == fixture.head_sha {
            evidence.blocked(
                "projects.fixture_refs",
                "夹具 BASE 与 PR head 是同一个 commit，无法区分冻结基线",
            );
            return Ok(None);
        }
        evidence.pass(
            "projects.fixture_manifest",
            format!(
                "夹具清单可用：仓库 {}，BASE {}，PR #{} head {}，API {}",
                fixture.repository_full_name,
                fixture.base_sha,
                fixture.pull_request,
                fixture.head_sha,
                fixture.github_api_base_url
            ),
        );
        Ok(Some(fixture))
    }

    /// 夹具里的 BASE 标记路径（`/project/repo` 下）。
    pub fn base_label_container_path(&self) -> String {
        format!("{REVIEW_CONTAINER_PATH}/{}", self.base_label_path)
    }

    /// 夹具里只有 PR head 才存在的标记路径：它在冻结的 BASE 视图里必须不存在。
    pub fn head_label_container_path(&self) -> String {
        format!("{REVIEW_CONTAINER_PATH}/{}", self.head_label_path)
    }
}

/// 环境变量指定的 GitHub API 地址（root 可用它覆盖夹具清单里的地址）。
pub const API_BASE_URL_ENV: &str = "MAI_ACCEPTANCE_GITHUB_API_BASE_URL";

/// 解析最终使用的 GitHub API 地址：环境变量优先，其次夹具清单。
pub fn resolve_api_base_url(fixture: &GitHubFixture) -> String {
    std::env::var(API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fixture.github_api_base_url.clone())
}
