use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use mai_protocol::now;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReviewSignalInput {
    pub(crate) pr: u64,
    pub(crate) head_sha: Option<String>,
    pub(crate) delivery_id: Option<String>,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingProjectReview {
    pub(crate) pr: u64,
    pub(crate) head_sha: Option<String>,
    pub(crate) delivery_id: Option<String>,
    pub(crate) reason: String,
    pub(crate) queued_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) update_count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectReviewPoolEnqueueSummary {
    pub(crate) queued: Vec<u64>,
    pub(crate) deduped: Vec<u64>,
    pub(crate) ignored: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectReviewPool {
    pending: BTreeMap<u64, PendingProjectReview>,
}

impl ProjectReviewPool {
    pub(crate) fn enqueue_many(
        &mut self,
        signals: impl IntoIterator<Item = ProjectReviewSignalInput>,
    ) -> ProjectReviewPoolEnqueueSummary {
        let mut summary = ProjectReviewPoolEnqueueSummary::default();
        for signal in signals {
            self.enqueue(signal, &mut summary);
        }
        summary
    }

    pub(crate) fn next(&mut self) -> Option<PendingProjectReview> {
        let pr = self.pending.first_key_value().map(|(pr, _)| *pr)?;
        self.pending.remove(&pr)
    }

    pub(crate) fn requeue(&mut self, pending: PendingProjectReview) {
        self.pending.entry(pending.pr).or_insert(pending);
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn enqueue(
        &mut self,
        signal: ProjectReviewSignalInput,
        summary: &mut ProjectReviewPoolEnqueueSummary,
    ) {
        if signal.pr == 0 {
            summary.ignored.push(signal.pr);
            return;
        }

        let updated_at = now();
        match self.pending.get_mut(&signal.pr) {
            Some(existing) => {
                existing.updated_at = updated_at;
                existing.update_count += 1;
                if signal.head_sha.is_some() {
                    existing.head_sha = signal.head_sha;
                }
                if signal.delivery_id.is_some() {
                    existing.delivery_id = signal.delivery_id;
                }
                if !signal.reason.trim().is_empty() {
                    existing.reason = signal.reason;
                }
                summary.deduped.push(existing.pr);
            }
            None => {
                self.pending.insert(
                    signal.pr,
                    PendingProjectReview {
                        pr: signal.pr,
                        head_sha: signal.head_sha,
                        delivery_id: signal.delivery_id,
                        reason: signal.reason,
                        queued_at: updated_at,
                        updated_at,
                        update_count: 1,
                    },
                );
                summary.queued.push(signal.pr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn same_pr_is_deduped() {
        let mut pool = ProjectReviewPool::default();

        let summary = pool.enqueue_many([
            signal(7, Some("old"), "webhook"),
            signal(7, Some("new"), "selector"),
        ]);

        assert_eq!(
            ProjectReviewPoolEnqueueSummary {
                queued: vec![7],
                deduped: vec![7],
                ignored: vec![],
            },
            summary
        );
        let pending = pool.next().expect("queued pr");
        assert_eq!(7, pending.pr);
        assert_eq!(Some("new".to_string()), pending.head_sha);
        assert_eq!("selector", pending.reason);
        assert_eq!(2, pending.update_count);
    }

    #[test]
    fn next_returns_prs_by_number() {
        let mut pool = ProjectReviewPool::default();

        pool.enqueue_many([
            signal(42, None, "selector"),
            signal(3, None, "webhook"),
            signal(17, None, "webhook"),
        ]);

        assert_eq!(Some(3), pool.next().map(|pending| pending.pr));
        assert_eq!(Some(17), pool.next().map(|pending| pending.pr));
        assert_eq!(Some(42), pool.next().map(|pending| pending.pr));
        assert_eq!(None, pool.next().map(|pending| pending.pr));
    }

    #[test]
    fn selector_and_webhook_share_dedupe_logic() {
        let mut pool = ProjectReviewPool::default();

        let selector = pool.enqueue_many([signal(5, Some("from-selector"), "selector")]);
        let webhook = pool.enqueue_many([signal(5, Some("from-webhook"), "webhook")]);

        assert_eq!(vec![5], selector.queued);
        assert_eq!(vec![5], webhook.deduped);

        let pending = pool.next().expect("queued pr");
        assert_eq!(Some("from-webhook".to_string()), pending.head_sha);
        assert_eq!("webhook", pending.reason);
    }

    #[test]
    fn clear_drops_pending_prs() {
        let mut pool = ProjectReviewPool::default();
        pool.enqueue_many([signal(1, None, "selector"), signal(2, None, "selector")]);

        pool.clear();

        assert_eq!(None, pool.next());
    }

    #[test]
    fn requeue_restores_claimed_pr_without_overwriting_newer_signal() {
        let mut pool = ProjectReviewPool::default();
        pool.enqueue_many([signal(9, Some("old"), "webhook")]);
        let pending = pool.next().expect("queued pr");

        pool.enqueue_many([signal(9, Some("new"), "webhook")]);
        pool.requeue(pending);

        let restored = pool.next().expect("requeued pr");
        assert_eq!(9, restored.pr);
        assert_eq!(Some("new".to_string()), restored.head_sha);
        assert_eq!(None, pool.next());
    }

    fn signal(pr: u64, head_sha: Option<&str>, reason: &str) -> ProjectReviewSignalInput {
        ProjectReviewSignalInput {
            pr,
            head_sha: head_sha.map(ToString::to_string),
            delivery_id: None,
            reason: reason.to_string(),
        }
    }
}
