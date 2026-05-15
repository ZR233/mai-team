mod download;
mod install;
mod release;

use crate::error::{RelayErrorKind, RelayResult};
use mai_protocol::{RelayUpdateActionResponse, RelayUpdateCheckRequest, RelayUpdateStatusResponse};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tempfile::Builder as TempDirBuilder;

const RELEASE_API_URL: &str = "https://api.github.com/repos/ZR233/mai-team/releases/latest";
const RELAY_ASSET_NAME: &str = "mai-relay-x86_64-unknown-linux-gnu.tar.gz";
const RELAY_CHECKSUM_NAME: &str = "mai-relay-x86_64-unknown-linux-gnu.tar.gz.sha256";
const MAX_DOWNLOAD_BYTES: u64 = 100 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 1024 * 1024;
const USER_AGENT_VALUE: &str = "mai-relay-updater";
const RESTART_DELAY: Duration = Duration::from_millis(800);
const UPDATE_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

static UPDATE_STATUS_CACHE: OnceLock<Mutex<Option<CachedUpdateStatus>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct CachedUpdateStatus {
    checked_at: Instant,
    status: RelayUpdateStatusResponse,
}

pub(crate) async fn check(
    http: &reqwest::Client,
    request: RelayUpdateCheckRequest,
) -> RelayResult<RelayUpdateStatusResponse> {
    if !request.force {
        if let Some(status) = fresh_cached_status() {
            return Ok(status);
        }
    }
    let current_version = current_version();
    match release::fetch_latest_release(http).await {
        Ok(release) => {
            let status = release::status_from_release(current_version, &release);
            store_cached_status(status.clone());
            Ok(status)
        }
        Err(error) => {
            let warning = format!("failed to check relay update: {error}");
            Ok(any_cached_status_with_warning(&warning)
                .unwrap_or_else(|| warning_status(current_version, warning)))
        }
    }
}

pub(crate) async fn apply(http: &reqwest::Client) -> RelayResult<RelayUpdateActionResponse> {
    if !release::self_update_supported() {
        return Err(RelayErrorKind::InvalidInput(
            "relay self-update is supported on linux x86_64 only".to_string(),
        ));
    }

    let github_release = release::fetch_latest_release(http).await?;
    let status = release::status_from_release(current_version(), &github_release);
    if !status.has_update {
        return Err(RelayErrorKind::InvalidInput(
            "mai-relay is already up to date".to_string(),
        ));
    }
    if !status.can_update {
        let message = status
            .warning
            .clone()
            .unwrap_or_else(|| "latest relay release cannot be installed on this host".to_string());
        return Err(RelayErrorKind::InvalidInput(message));
    }

    let assets = release::select_release_assets(&github_release)?;
    let executable_path = install::current_executable_path()?;
    let executable_dir = executable_path.parent().ok_or_else(|| {
        RelayErrorKind::InvalidInput("relay executable has no parent directory".to_string())
    })?;
    let temp_dir = TempDirBuilder::new()
        .prefix("mai-relay-update-")
        .tempdir_in(executable_dir)?;
    let archive_path = temp_dir.path().join(RELAY_ASSET_NAME);
    let new_binary_path = temp_dir.path().join("mai-relay.new");

    download::download_verified_binary(
        http,
        &assets,
        &archive_path,
        &new_binary_path,
        MAX_DOWNLOAD_BYTES,
        MAX_CHECKSUM_BYTES,
    )
    .await?;
    install::set_executable_permissions(&new_binary_path)?;
    install::replace_binary(&executable_path, &new_binary_path)?;

    let mut status = status;
    status.restart_scheduled = true;
    install::schedule_restart(RESTART_DELAY);
    Ok(RelayUpdateActionResponse {
        status,
        message: format!(
            "mai-relay updated to {}",
            release::normalize_version(&github_release.tag_name)
        ),
        restart_scheduled: true,
    })
}

pub(crate) fn rollback() -> RelayResult<RelayUpdateActionResponse> {
    if !release::self_update_supported() {
        return Err(RelayErrorKind::InvalidInput(
            "relay self-update is supported on linux x86_64 only".to_string(),
        ));
    }
    let executable_path = install::current_executable_path()?;
    let backup_path = install::backup_path_for(&executable_path)?;
    if !backup_path.exists() {
        return Err(RelayErrorKind::InvalidInput(
            "no relay update backup is available".to_string(),
        ));
    }
    install::rollback_binary(&executable_path, &backup_path)?;
    install::schedule_restart(RESTART_DELAY);
    Ok(RelayUpdateActionResponse {
        status: local_status(true),
        message: "mai-relay rollback restored the backup and will restart".to_string(),
        restart_scheduled: true,
    })
}

pub(crate) fn restart() -> RelayUpdateActionResponse {
    install::schedule_restart(RESTART_DELAY);
    RelayUpdateActionResponse {
        status: local_status(true),
        message: "mai-relay restart scheduled".to_string(),
        restart_scheduled: true,
    }
}

fn fresh_cached_status() -> Option<RelayUpdateStatusResponse> {
    let cache = update_status_cache();
    let guard = cache.lock().ok()?;
    let cached = guard.as_ref()?;
    if cached.checked_at.elapsed() > UPDATE_CACHE_TTL {
        return None;
    }
    let mut status = cached.status.clone();
    status.cached = true;
    Some(status)
}

fn any_cached_status_with_warning(warning: &str) -> Option<RelayUpdateStatusResponse> {
    let cache = update_status_cache();
    let guard = cache.lock().ok()?;
    let cached = guard.as_ref()?;
    let mut status = cached.status.clone();
    status.cached = true;
    status.warning = Some(warning.to_string());
    Some(status)
}

fn store_cached_status(status: RelayUpdateStatusResponse) {
    let cache = update_status_cache();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedUpdateStatus {
            checked_at: Instant::now(),
            status,
        });
    }
}

fn update_status_cache() -> &'static Mutex<Option<CachedUpdateStatus>> {
    UPDATE_STATUS_CACHE.get_or_init(|| Mutex::new(None))
}

fn warning_status(current_version: String, warning: String) -> RelayUpdateStatusResponse {
    RelayUpdateStatusResponse {
        current_version: current_version.clone(),
        latest_version: current_version,
        has_update: false,
        can_update: false,
        release: None,
        cached: false,
        warning: Some(warning),
        restart_scheduled: false,
    }
}

fn local_status(restart_scheduled: bool) -> RelayUpdateStatusResponse {
    RelayUpdateStatusResponse {
        current_version: current_version(),
        latest_version: current_version(),
        has_update: false,
        can_update: release::self_update_supported(),
        release: None,
        cached: false,
        warning: None,
        restart_scheduled,
    }
}

fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
