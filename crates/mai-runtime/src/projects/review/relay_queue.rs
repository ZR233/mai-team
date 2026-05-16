use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use mai_protocol::now;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectReviewRelaySignalInput {
    pub(crate) pr: u64,
    pub(crate) head_sha: Option<String>,
    pub(crate) delivery_id: Option<String>,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingProjectReviewRelaySignal {
    pub(crate) pr: u64,
    pub(crate) head_sha: Option<String>,
    pub(crate) delivery_id: Option<String>,
    pub(crate) reason: String,
    pub(crate) queued_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) update_count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectReviewRelayQueueEnqueueSummary {
    pub(crate) queued: Vec<u64>,
    pub(crate) deduped: Vec<u64>,
    pub(crate) ignored: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectReviewRelayQueue {
    pending: BTreeMap<u64, PendingProjectReviewRelaySignal>,
}

impl ProjectReviewRelayQueue {
    pub(crate) fn enqueue_many(
        &mut self,
        signals: impl IntoIterator<Item = ProjectReviewRelaySignalInput>,
    ) -> ProjectReviewRelayQueueEnqueueSummary {
        let mut summary = ProjectReviewRelayQueueEnqueueSummary::default();
        for signal in signals {
            self.enqueue(signal, &mut summary);
        }
        summary
    }

    pub(crate) fn next(&mut self) -> Option<PendingProjectReviewRelaySignal> {
        let pr = self.pending.first_key_value().map(|(pr, _)| *pr)?;
        self.pending.remove(&pr)
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    fn enqueue(
        &mut self,
        signal: ProjectReviewRelaySignalInput,
        summary: &mut ProjectReviewRelayQueueEnqueueSummary,
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
                existing.head_sha = signal.head_sha;
                existing.delivery_id = signal.delivery_id;
                if !signal.reason.trim().is_empty() {
                    existing.reason = signal.reason;
                }
                summary.deduped.push(existing.pr);
            }
            None => {
                self.pending.insert(
                    signal.pr,
                    PendingProjectReviewRelaySignal {
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
    fn same_pr_is_deduped_with_latest_signal() {
        let mut queue = ProjectReviewRelayQueue::default();

        let summary = queue.enqueue_many([
            signal(7, Some("old"), Some("delivery-1"), "pull_request"),
            signal(7, Some("new"), Some("delivery-2"), "check_run"),
        ]);

        assert_eq!(
            ProjectReviewRelayQueueEnqueueSummary {
                queued: vec![7],
                deduped: vec![7],
                ignored: vec![],
            },
            summary
        );
        let pending = queue.next().expect("queued relay signal");
        assert_eq!(7, pending.pr);
        assert_eq!(Some("new".to_string()), pending.head_sha);
        assert_eq!(Some("delivery-2".to_string()), pending.delivery_id);
        assert_eq!("check_run", pending.reason);
        assert_eq!(2, pending.update_count);
    }

    #[test]
    fn next_returns_prs_by_number() {
        let mut queue = ProjectReviewRelayQueue::default();

        queue.enqueue_many([
            signal(42, None, None, "check_suite"),
            signal(3, None, None, "pull_request"),
            signal(17, None, None, "check_run"),
        ]);

        assert_eq!(Some(3), queue.next().map(|pending| pending.pr));
        assert_eq!(Some(17), queue.next().map(|pending| pending.pr));
        assert_eq!(Some(42), queue.next().map(|pending| pending.pr));
        assert_eq!(None, queue.next().map(|pending| pending.pr));
    }

    #[test]
    fn clear_drops_pending_relay_signals() {
        let mut queue = ProjectReviewRelayQueue::default();
        queue.enqueue_many([signal(1, None, None, "pull_request")]);

        queue.clear();

        assert_eq!(None, queue.next());
    }

    fn signal(
        pr: u64,
        head_sha: Option<&str>,
        delivery_id: Option<&str>,
        reason: &str,
    ) -> ProjectReviewRelaySignalInput {
        ProjectReviewRelaySignalInput {
            pr,
            head_sha: head_sha.map(ToString::to_string),
            delivery_id: delivery_id.map(ToString::to_string),
            reason: reason.to_string(),
        }
    }
}
