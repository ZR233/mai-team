use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::error::{DockerError, Result};
use crate::image::{ImageRefreshFailure, ImageRefreshOutcome, floating_latest_reference};

#[derive(Debug, Clone)]
pub struct DockerClient {
    pub(crate) binary: String,
    pub(crate) image: String,
    image_refreshes: Arc<Mutex<HashMap<String, Arc<ImageRefreshFlight>>>>,
}

type ImageRefreshResult = std::result::Result<ImageRefreshOutcome, ImageRefreshFailure>;

#[derive(Debug)]
struct ImageRefreshFlight {
    result: Mutex<Option<ImageRefreshResult>>,
    notify: Notify,
    cancellation: CancellationToken,
    waiters: AtomicUsize,
}

impl ImageRefreshFlight {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            notify: Notify::new(),
            cancellation: CancellationToken::new(),
            waiters: AtomicUsize::new(0),
        }
    }
}

struct ImageRefreshWaiter {
    flight: Arc<ImageRefreshFlight>,
}

impl ImageRefreshWaiter {
    fn new(flight: Arc<ImageRefreshFlight>) -> Self {
        flight.waiters.fetch_add(1, Ordering::Relaxed);
        Self { flight }
    }
}

impl Drop for ImageRefreshWaiter {
    fn drop(&mut self) {
        if self.flight.waiters.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.flight.cancellation.cancel();
        }
    }
}

impl DockerClient {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            binary: "docker".to_string(),
            image: image.into(),
            image_refreshes: Arc::default(),
        }
    }

    pub fn new_with_binary(image: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            image: image.into(),
            image_refreshes: Arc::default(),
        }
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub async fn check_available(&self) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(["version", "--format", "{{.Server.Version}}"])
            .output()
            .await
            .map_err(|err| DockerError::NotAvailable(err.to_string()))?;
        if !output.status.success() {
            return Err(DockerError::NotAvailable(stderr_or_stdout(&output)));
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    pub async fn refresh_floating_latest_image(
        &self,
        image: &str,
        timeout: Duration,
    ) -> Result<ImageRefreshOutcome> {
        let Some(image) = floating_latest_reference(image)? else {
            return Ok(ImageRefreshOutcome::NotRequired {
                image: image.to_string(),
            });
        };
        let (refresh, starts_refresh) = {
            let mut refreshes = self.image_refreshes.lock().await;
            match refreshes.entry(image.clone()) {
                Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
                Entry::Vacant(entry) => {
                    let refresh = Arc::new(ImageRefreshFlight::new());
                    entry.insert(Arc::clone(&refresh));
                    (refresh, true)
                }
            }
        };
        let _waiter = ImageRefreshWaiter::new(Arc::clone(&refresh));
        if starts_refresh {
            let client = self.clone();
            let refresh = Arc::clone(&refresh);
            let image = image.clone();
            tokio::spawn(async move {
                let result = client
                    .refresh_floating_latest_image_once(
                        image.clone(),
                        timeout,
                        &refresh.cancellation,
                    )
                    .await;
                *refresh.result.lock().await = Some(result);
                refresh.notify.notify_waiters();
                let mut refreshes = client.image_refreshes.lock().await;
                if refreshes
                    .get(&image)
                    .is_some_and(|current| Arc::ptr_eq(current, &refresh))
                {
                    refreshes.remove(&image);
                }
            });
        }
        loop {
            let notified = refresh.notify.notified();
            if let Some(result) = refresh.result.lock().await.clone() {
                return result.map_err(ImageRefreshFailure::into_docker_error);
            }
            notified.await;
        }
    }

    async fn refresh_floating_latest_image_once(
        &self,
        image: String,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> ImageRefreshResult {
        let before = self.inspect_image_id(&image, timeout, cancellation).await;
        let started = std::time::Instant::now();
        let pull = self
            .docker_output(
                ["pull", image.as_str()],
                timeout,
                "docker image pull",
                cancellation,
            )
            .await;
        let elapsed = started.elapsed();
        if let Err(error) = pull {
            let cached_image_id = self.inspect_image_id(&image, timeout, cancellation).await;
            return match cached_image_id {
                Some(image_id) => Ok(ImageRefreshOutcome::CachedFallback {
                    image,
                    image_id,
                    elapsed,
                    error: error.to_string(),
                }),
                None => Err(ImageRefreshFailure::from_docker_error(error)),
            };
        }
        let image_id = self
            .inspect_image_id(&image, timeout, cancellation)
            .await
            .ok_or_else(|| {
                ImageRefreshFailure::CommandFailed(format!(
                    "docker pull succeeded but image {image} could not be inspected"
                ))
            })?;
        if before.as_deref() == Some(image_id.as_str()) {
            Ok(ImageRefreshOutcome::UpToDate {
                image,
                image_id,
                elapsed,
            })
        } else {
            Ok(ImageRefreshOutcome::Updated {
                image,
                previous_image_id: before,
                image_id,
                elapsed,
            })
        }
    }

    async fn inspect_image_id(
        &self,
        image: &str,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Option<String> {
        let output = self
            .docker_output(
                ["image", "inspect", "--format", "{{.Id}}", image],
                timeout,
                "docker image inspect",
                cancellation,
            )
            .await
            .ok()?;
        String::from_utf8(output.stdout)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    async fn docker_output<const N: usize>(
        &self,
        args: [&str; N],
        timeout: Duration,
        description: &str,
        cancellation: &CancellationToken,
    ) -> Result<std::process::Output> {
        let mut command = Command::new(&self.binary);
        command.args(args).kill_on_drop(true);
        let output = tokio::select! {
            output = tokio::time::timeout(timeout, command.output()) => {
                output.map_err(|_| {
                    DockerError::CommandFailed(format!(
                        "{description} timed out after {}s",
                        timeout.as_secs()
                    ))
                })??
            }
            _ = cancellation.cancelled() => return Err(DockerError::Cancelled),
        };
        if !output.status.success() {
            return Err(DockerError::CommandFailed(stderr_or_stdout(&output)));
        }
        Ok(output)
    }
}

pub(crate) fn stderr_or_stdout(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::DockerClient;
    use crate::{DockerError, ImageRefreshOutcome};

    struct FakeDocker {
        _directory: TempDir,
        binary: String,
        current: std::path::PathBuf,
        desired: std::path::PathBuf,
        fail_pull: std::path::PathBuf,
        pull_delay: std::path::PathBuf,
        calls: std::path::PathBuf,
    }

    impl FakeDocker {
        fn new() -> Self {
            let directory = TempDir::new().expect("temporary directory");
            let binary = directory.path().join("docker");
            let binary_staging = directory.path().join("docker.new");
            let current = directory.path().join("current");
            let desired = directory.path().join("desired");
            let fail_pull = directory.path().join("fail-pull");
            let pull_delay = directory.path().join("pull-delay");
            let calls = directory.path().join("calls");
            fs::write(
                &binary_staging,
                format!(
                    "#!/bin/sh\n\
                     echo \"$*\" >> '{}'\n\
                     if [ \"$1\" = image ] && [ \"$2\" = inspect ]; then\n\
                       test -s '{}' || exit 1\n\
                       cat '{}'\n\
                       exit 0\n\
                     fi\n\
                     if [ \"$1\" = pull ]; then\n\
                       if [ -s '{}' ]; then sleep \"$(cat '{}')\"; else sleep 0.1; fi\n\
                       if [ -e '{}' ]; then echo 'registry unavailable' >&2; exit 1; fi\n\
                       cp '{}' '{}'\n\
                       exit 0\n\
                     fi\n\
                     exit 2\n",
                    calls.display(),
                    current.display(),
                    current.display(),
                    pull_delay.display(),
                    pull_delay.display(),
                    fail_pull.display(),
                    desired.display(),
                    current.display(),
                ),
            )
            .expect("write fake docker");
            let mut permissions = fs::metadata(&binary_staging)
                .expect("fake docker metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&binary_staging, permissions).expect("make fake docker executable");
            fs::rename(&binary_staging, &binary).expect("publish fake docker atomically");
            Self {
                _directory: directory,
                binary: binary.to_string_lossy().into_owned(),
                current,
                desired,
                fail_pull,
                pull_delay,
                calls,
            }
        }

        fn client(&self) -> DockerClient {
            DockerClient::new_with_binary("unused:latest", self.binary.clone())
        }

        fn set_current(&self, image_id: &str) {
            fs::write(&self.current, image_id).expect("write current image");
        }

        fn set_desired(&self, image_id: &str) {
            fs::write(&self.desired, image_id).expect("write desired image");
        }

        fn fail_pull(&self) {
            fs::write(&self.fail_pull, "").expect("enable pull failure");
        }

        fn set_pull_delay(&self, seconds: &str) {
            fs::write(&self.pull_delay, seconds).expect("write pull delay");
        }

        fn calls(&self) -> String {
            fs::read_to_string(&self.calls).unwrap_or_default()
        }
    }

    #[tokio::test]
    async fn refreshes_implicit_latest_and_reports_changed_image() {
        let docker = FakeDocker::new();
        docker.set_current("sha256:old");
        docker.set_desired("sha256:new");

        let outcome = docker
            .client()
            .refresh_floating_latest_image("localhost:5000/reviewer", Duration::from_secs(2))
            .await
            .expect("refresh image");

        assert!(matches!(
            outcome,
            ImageRefreshOutcome::Updated {
                image,
                previous_image_id: Some(previous),
                image_id,
                ..
            } if image == "localhost:5000/reviewer:latest"
                && previous == "sha256:old"
                && image_id == "sha256:new"
        ));
        assert!(
            docker
                .calls()
                .contains("pull localhost:5000/reviewer:latest")
        );
    }

    #[tokio::test]
    async fn uses_cached_image_when_pull_fails() {
        let docker = FakeDocker::new();
        docker.set_current("sha256:cached");
        docker.set_desired("sha256:new");
        docker.fail_pull();

        let outcome = docker
            .client()
            .refresh_floating_latest_image("reviewer:latest", Duration::from_secs(2))
            .await
            .expect("cached fallback");

        assert!(matches!(
            outcome,
            ImageRefreshOutcome::CachedFallback {
                image_id,
                error,
                ..
            } if image_id == "sha256:cached" && error.contains("registry unavailable")
        ));
    }

    #[tokio::test]
    async fn fails_when_pull_and_local_cache_are_unavailable() {
        let docker = FakeDocker::new();
        docker.set_desired("sha256:new");
        docker.fail_pull();

        let error = docker
            .client()
            .refresh_floating_latest_image("reviewer:latest", Duration::from_secs(2))
            .await
            .expect_err("missing image must fail");

        assert!(matches!(
            error,
            DockerError::CommandFailed(message) if message.contains("registry unavailable")
        ));
    }

    #[tokio::test]
    async fn concurrent_refreshes_share_one_pull() {
        let docker = FakeDocker::new();
        docker.set_current("sha256:old");
        docker.set_desired("sha256:new");
        let client = docker.client();

        let (first, second) = tokio::join!(
            client.refresh_floating_latest_image("reviewer:latest", Duration::from_secs(2)),
            client.refresh_floating_latest_image("reviewer:latest", Duration::from_secs(2)),
        );

        assert_eq!(
            first.expect("first refresh"),
            second.expect("second refresh")
        );
        assert_eq!(
            1,
            docker
                .calls()
                .lines()
                .filter(|line| line.starts_with("pull "))
                .count()
        );
    }

    #[tokio::test]
    async fn pull_timeout_fails_without_a_cached_image() {
        let docker = FakeDocker::new();
        docker.set_desired("sha256:new");
        docker.set_pull_delay("2");

        let error = docker
            .client()
            .refresh_floating_latest_image("reviewer:latest", Duration::from_millis(50))
            .await
            .expect_err("timed out pull");

        assert!(matches!(
            error,
            DockerError::CommandFailed(message) if message.contains("timed out")
        ));
    }

    #[tokio::test]
    async fn cancelling_last_waiter_stops_and_removes_the_refresh_flight() {
        let docker = FakeDocker::new();
        docker.set_desired("sha256:new");
        docker.set_pull_delay("2");
        let client = docker.client();
        let refresh_client = client.clone();
        let refresh = tokio::spawn(async move {
            refresh_client
                .refresh_floating_latest_image("reviewer:latest", Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        refresh.abort();
        let _ = refresh.await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if client.image_refreshes.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled refresh flight cleanup");
    }
}
