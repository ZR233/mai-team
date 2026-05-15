use crate::error::{RelayErrorKind, RelayResult};
use mai_protocol::{RelayUpdateReleaseInfo, RelayUpdateStatusResponse};
use reqwest::Url;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::{Deserialize, Deserializer};
use std::cmp::Ordering;

use super::{
    MAX_DOWNLOAD_BYTES, RELAY_ASSET_NAME, RELAY_CHECKSUM_NAME, RELEASE_API_URL, USER_AGENT_VALUE,
};

#[derive(Debug, Deserialize)]
pub(super) struct GithubRelease {
    pub(super) tag_name: String,
    #[serde(default, deserialize_with = "empty_string_from_null")]
    name: String,
    #[serde(default, deserialize_with = "empty_string_from_null")]
    body: String,
    #[serde(default, deserialize_with = "empty_string_from_null")]
    published_at: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SelectedReleaseAssets {
    pub(super) archive_url: String,
    pub(super) checksum_url: String,
}

fn empty_string_from_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

pub(super) async fn fetch_latest_release(http: &reqwest::Client) -> RelayResult<GithubRelease> {
    Ok(http
        .get(RELEASE_API_URL)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GithubRelease>()
        .await?)
}

pub(super) fn status_from_release(
    current_version: String,
    release: &GithubRelease,
) -> RelayUpdateStatusResponse {
    let latest_version = normalize_version(&release.tag_name);
    let has_update = compare_versions(&current_version, &latest_version) == Ordering::Less;
    let platform_supported = self_update_supported();
    let asset_result = select_release_assets(release);
    let warning = match (platform_supported, asset_result.as_ref()) {
        (false, _) => Some("relay self-update is supported on linux x86_64 only".to_string()),
        (true, Err(error)) => Some(error.to_string()),
        (true, Ok(_)) => None,
    };
    RelayUpdateStatusResponse {
        current_version,
        latest_version,
        has_update,
        can_update: platform_supported && has_update && asset_result.is_ok(),
        release: Some(RelayUpdateReleaseInfo {
            name: release.name.clone(),
            body: release.body.clone(),
            published_at: release.published_at.clone(),
            html_url: release.html_url.clone(),
        }),
        cached: false,
        warning,
        restart_scheduled: false,
    }
}

pub(super) fn self_update_supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

pub(super) fn normalize_version(value: &str) -> String {
    value.trim().trim_start_matches('v').to_string()
}

pub(super) fn select_release_assets(release: &GithubRelease) -> RelayResult<SelectedReleaseAssets> {
    let archive = release
        .assets
        .iter()
        .find(|asset| asset.name == RELAY_ASSET_NAME)
        .ok_or_else(|| {
            RelayErrorKind::InvalidInput(format!(
                "release {} does not include {RELAY_ASSET_NAME}",
                release.tag_name
            ))
        })?;
    if archive.size > MAX_DOWNLOAD_BYTES {
        return Err(RelayErrorKind::InvalidInput(format!(
            "{RELAY_ASSET_NAME} is larger than 100 MB"
        )));
    }
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name == RELAY_CHECKSUM_NAME)
        .ok_or_else(|| {
            RelayErrorKind::InvalidInput(format!(
                "release {} does not include {RELAY_CHECKSUM_NAME}",
                release.tag_name
            ))
        })?;
    validate_download_url(&archive.browser_download_url)?;
    validate_download_url(&checksum.browser_download_url)?;
    Ok(SelectedReleaseAssets {
        archive_url: archive.browser_download_url.clone(),
        checksum_url: checksum.browser_download_url.clone(),
    })
}

pub(super) fn validate_download_url(url: &str) -> RelayResult<Url> {
    let parsed =
        Url::parse(url).map_err(|error| RelayErrorKind::InvalidInput(error.to_string()))?;
    if parsed.scheme() != "https" {
        return Err(RelayErrorKind::InvalidInput(
            "relay update downloads must use https".to_string(),
        ));
    }
    match parsed.host_str() {
        Some("github.com") | Some("objects.githubusercontent.com") => Ok(parsed),
        Some(host) => Err(RelayErrorKind::InvalidInput(format!(
            "relay update download host `{host}` is not trusted"
        ))),
        None => Err(RelayErrorKind::InvalidInput(
            "relay update download URL has no host".to_string(),
        )),
    }
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let left_parts = version_parts(left);
    let right_parts = version_parts(right);
    let len = left_parts.len().max(right_parts.len());
    for index in 0..len {
        let left_part = left_parts.get(index).copied().unwrap_or_default();
        let right_part = right_parts.get(index).copied().unwrap_or_default();
        match left_part.cmp(&right_part) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    Ordering::Equal
}

fn version_parts(value: &str) -> Vec<u64> {
    normalize_version(value)
        .split(['.', '-', '+'])
        .map(|part| {
            part.chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or_default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn compare_versions_uses_numeric_segments() {
        assert_eq!(compare_versions("0.10.0", "0.2.0"), Ordering::Greater);
        assert_eq!(compare_versions("v1.2.0", "1.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.2.0", "1.2.1"), Ordering::Less);
        assert_eq!(compare_versions("1.2.3", "1.2"), Ordering::Greater);
    }

    #[test]
    fn select_release_assets_finds_linux_archive_and_checksum() {
        let release = release_with_assets(vec![
            asset(
                "notes.txt",
                "https://github.com/ZR233/mai-team/releases/download/v0.1.0/notes.txt",
                10,
            ),
            asset(
                RELAY_ASSET_NAME,
                "https://github.com/ZR233/mai-team/releases/download/v0.1.0/mai-relay.tar.gz",
                100,
            ),
            asset(
                RELAY_CHECKSUM_NAME,
                "https://github.com/ZR233/mai-team/releases/download/v0.1.0/mai-relay.tar.gz.sha256",
                64,
            ),
        ]);

        let selected = select_release_assets(&release).expect("selected assets");

        assert_eq!(
            selected,
            SelectedReleaseAssets {
                archive_url:
                    "https://github.com/ZR233/mai-team/releases/download/v0.1.0/mai-relay.tar.gz"
                        .to_string(),
                checksum_url:
                    "https://github.com/ZR233/mai-team/releases/download/v0.1.0/mai-relay.tar.gz.sha256"
                        .to_string(),
            }
        );
    }

    #[test]
    fn missing_release_asset_reports_update_without_update_capability() {
        let release = release_with_assets(Vec::new());

        let status = status_from_release("0.1.0".to_string(), &release);

        assert!(status.has_update);
        assert!(!status.can_update);
        assert!(
            status
                .warning
                .unwrap_or_default()
                .contains(RELAY_ASSET_NAME)
        );
    }

    #[test]
    fn github_release_deserialization_accepts_null_body() {
        let release: GithubRelease = serde_json::from_value(serde_json::json!({
            "tag_name": "v0.2.0",
            "name": "v0.2.0",
            "body": null,
            "published_at": "2026-05-15T00:00:00Z",
            "html_url": "https://github.com/ZR233/mai-team/releases/tag/v0.2.0",
            "assets": []
        }))
        .expect("deserialize release");

        assert_eq!(release.body, "");
    }

    #[test]
    fn validate_download_url_accepts_only_trusted_https_hosts() {
        assert!(
            validate_download_url(
                "https://github.com/ZR233/mai-team/releases/download/v0.1.0/mai-relay.tar.gz"
            )
            .is_ok()
        );
        assert!(
            validate_download_url(
                "https://objects.githubusercontent.com/github-production-release-asset/file"
            )
            .is_ok()
        );
        assert!(validate_download_url("http://github.com/ZR233/mai-team/releases/file").is_err());
        assert!(validate_download_url("https://example.com/file").is_err());
    }

    fn release_with_assets(assets: Vec<GithubReleaseAsset>) -> GithubRelease {
        GithubRelease {
            tag_name: "v0.2.0".to_string(),
            name: "v0.2.0".to_string(),
            body: "release".to_string(),
            published_at: "2026-05-15T00:00:00Z".to_string(),
            html_url: "https://github.com/ZR233/mai-team/releases/tag/v0.2.0".to_string(),
            assets,
        }
    }

    fn asset(name: &str, url: &str, size: u64) -> GithubReleaseAsset {
        GithubReleaseAsset {
            name: name.to_string(),
            browser_download_url: url.to_string(),
            size,
        }
    }
}
