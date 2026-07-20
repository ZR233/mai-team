use std::future::Future;

use tokio::time::{Duration, sleep};

use crate::{Result, RuntimeError};

const GITHUB_REQUEST_MAX_ATTEMPTS: usize = 4;
const GITHUB_REQUEST_INITIAL_RETRY_MILLIS: u64 = 200;
const GITHUB_REQUEST_MAX_RETRY_SECS: u64 = 2;

#[derive(Debug, Clone, Copy)]
struct GithubRetryPolicy {
    max_attempts: usize,
    initial_delay: Duration,
    max_delay: Duration,
}

impl Default for GithubRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: GITHUB_REQUEST_MAX_ATTEMPTS,
            initial_delay: Duration::from_millis(GITHUB_REQUEST_INITIAL_RETRY_MILLIS),
            max_delay: Duration::from_secs(GITHUB_REQUEST_MAX_RETRY_SECS),
        }
    }
}

pub(crate) async fn retry_github_request<T, Request, RequestFuture>(
    operation: &str,
    request: Request,
) -> Result<T>
where
    Request: FnMut() -> RequestFuture,
    RequestFuture: Future<Output = Result<T>>,
{
    retry_github_request_with_policy(operation, request, GithubRetryPolicy::default()).await
}

async fn retry_github_request_with_policy<T, Request, RequestFuture>(
    operation: &str,
    mut request: Request,
    policy: GithubRetryPolicy,
) -> Result<T>
where
    Request: FnMut() -> RequestFuture,
    RequestFuture: Future<Output = Result<T>>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut next_delay = policy.initial_delay;
    for attempt in 1..=max_attempts {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error) if github_request_error_is_retryable(&error) && attempt < max_attempts => {
                let delay = github_retry_after(&error)
                    .unwrap_or(next_delay)
                    .min(policy.max_delay);
                tracing::warn!(
                    operation,
                    attempt,
                    max_attempts,
                    delay_ms = delay.as_millis(),
                    error = %error,
                    "retrying transient GitHub request"
                );
                sleep(delay).await;
                next_delay = next_delay.saturating_mul(2).min(policy.max_delay);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("GitHub retry loop always returns from an attempt")
}

pub(crate) fn github_request_error_is_retryable(error: &RuntimeError) -> bool {
    if matches!(error, RuntimeError::GithubUnavailable { .. }) {
        return true;
    }
    if let RuntimeError::Http(error) = error {
        return error.is_timeout() || error.is_connect() || error.is_request() || error.is_body();
    }
    if let RuntimeError::InvalidInput(message) = error {
        return github_error_message_is_retryable(message);
    }
    false
}

pub(crate) fn github_error_message_is_retryable(message: &str) -> bool {
    let message = message.trim().to_ascii_lowercase();
    if matches!(
        message.strip_prefix("invalid input: ").unwrap_or(&message),
        "relay is not connected"
            | "relay is enabled but not connected"
            | "relay connection closed"
            | "relay request timed out"
    ) {
        return true;
    }
    let github_operation = message.contains("github")
        || message.contains("installation token")
        || message.contains("installation_token");
    github_operation
        && [
            "408 request timeout",
            "429 too many requests",
            "500 internal server error",
            "502 bad gateway",
            "503 service unavailable",
            "504 gateway timeout",
        ]
        .iter()
        .any(|marker| message.contains(marker))
}

fn github_retry_after(error: &RuntimeError) -> Option<Duration> {
    if let RuntimeError::GithubUnavailable { retry_after, .. } = error {
        return *retry_after;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pretty_assertions::assert_eq;
    use reqwest::StatusCode;

    use super::*;

    fn no_delay_policy(max_attempts: usize) -> GithubRetryPolicy {
        GithubRetryPolicy {
            max_attempts,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn transient_github_failure_is_retried() {
        let attempts = AtomicUsize::new(0);

        let value = retry_github_request_with_policy(
            "read pull request",
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        return Err(RuntimeError::GithubUnavailable {
                            operation: "read pull request".to_string(),
                            status: StatusCode::SERVICE_UNAVAILABLE,
                            message: "temporarily unavailable".to_string(),
                            retry_after: None,
                        });
                    }
                    Ok(42)
                }
            },
            no_delay_policy(3),
        )
        .await
        .expect("retry succeeds");

        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn permanent_github_failure_is_not_retried() {
        let attempts = AtomicUsize::new(0);

        let error = retry_github_request_with_policy(
            "read pull request",
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(RuntimeError::InvalidInput("not found".to_string())) }
            },
            no_delay_policy(3),
        )
        .await
        .expect_err("permanent error");

        assert_eq!(error.to_string(), "invalid input: not found");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn relay_github_service_unavailable_message_is_retryable() {
        assert!(github_error_message_is_retryable(
            "relay invalid_input failed: invalid input: create installation token failed with 503 Service Unavailable"
        ));
        assert!(!github_error_message_is_retryable(
            "GitHub read project GitHub API failed (404 Not Found): missing"
        ));
    }
}
