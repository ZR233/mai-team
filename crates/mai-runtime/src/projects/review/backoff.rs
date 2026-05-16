use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReviewRetryBackoffConfig {
    pub(crate) initial_delay: Duration,
    pub(crate) max_delay: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectReviewRetryBackoff {
    config: ProjectReviewRetryBackoffConfig,
    next_delay: Duration,
}

impl ProjectReviewRetryBackoff {
    pub(crate) fn new(config: ProjectReviewRetryBackoffConfig) -> Self {
        Self {
            next_delay: bounded_delay(config.initial_delay, config.max_delay),
            config,
        }
    }

    pub(crate) fn next_delay(&mut self) -> Duration {
        let delay = bounded_delay(self.next_delay, self.config.max_delay);
        self.next_delay = doubled_delay(delay, self.config.max_delay);
        delay
    }

    pub(crate) fn reset(&mut self) {
        self.next_delay = bounded_delay(self.config.initial_delay, self.config.max_delay);
    }
}

fn bounded_delay(delay: Duration, max_delay: Duration) -> Duration {
    if delay > max_delay { max_delay } else { delay }
}

fn doubled_delay(delay: Duration, max_delay: Duration) -> Duration {
    delay.checked_mul(2).unwrap_or(max_delay).min(max_delay)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn exponential_backoff_doubles_until_cap_and_resets() {
        let mut backoff = ProjectReviewRetryBackoff::new(ProjectReviewRetryBackoffConfig {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
        });

        assert_eq!(Duration::from_secs(1), backoff.next_delay());
        assert_eq!(Duration::from_secs(2), backoff.next_delay());
        assert_eq!(Duration::from_secs(4), backoff.next_delay());
        assert_eq!(Duration::from_secs(8), backoff.next_delay());
        assert_eq!(Duration::from_secs(8), backoff.next_delay());

        backoff.reset();

        assert_eq!(Duration::from_secs(1), backoff.next_delay());
    }
}
