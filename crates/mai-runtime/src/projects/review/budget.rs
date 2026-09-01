use std::time::Duration;

/// Review 模式一次 PL Turn 的活跃墙钟预算。
pub(crate) const REVIEW_TURN_BUDGET: Duration = Duration::from_secs(60 * 60);

/// PL Turn 结束后留给归档、回执确认与资源清理的外层收尾余量。
const REVIEW_FINALIZATION_GRACE: Duration = Duration::from_secs(5 * 60);

/// Review Job 对单次 Running 尝试的最终资源保护期限。
pub(crate) const REVIEW_RUNNING_DEADLINE: Duration =
    REVIEW_TURN_BUDGET.saturating_add(REVIEW_FINALIZATION_GRACE);

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::{REVIEW_FINALIZATION_GRACE, REVIEW_RUNNING_DEADLINE, REVIEW_TURN_BUDGET};

    #[test]
    fn review_turn_has_one_hour_budget_and_separate_finalization_grace() {
        assert_eq!(REVIEW_TURN_BUDGET, std::time::Duration::from_secs(60 * 60));
        assert_eq!(
            REVIEW_RUNNING_DEADLINE - REVIEW_TURN_BUDGET,
            REVIEW_FINALIZATION_GRACE
        );
    }
}
