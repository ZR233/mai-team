use std::time::Duration;

/// Review Job 单次 Running 尝试的产品级资源保护期限。
///
/// 主代理的 PL Turn 不设预算；这里只防止外部模型、工具或网络长期无终态。
pub(crate) const REVIEW_RUNNING_DEADLINE: Duration = Duration::from_secs(65 * 60);

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::REVIEW_RUNNING_DEADLINE;

    #[test]
    fn review_running_watchdog_has_product_resource_deadline() {
        assert_eq!(
            REVIEW_RUNNING_DEADLINE,
            std::time::Duration::from_secs(65 * 60)
        );
    }
}
