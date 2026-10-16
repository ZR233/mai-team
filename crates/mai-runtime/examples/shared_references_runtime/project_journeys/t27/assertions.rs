//! T27 的判定辅助。

use crate::evidence::Evidence;

/// 记录一条布尔判定并返回判定值，供调用方继续收集完整失败证据。
pub(super) fn checked(
    evidence: &mut Evidence,
    id: &str,
    condition: bool,
    pass: String,
    failure: String,
) -> bool {
    evidence.check(id, condition, pass, failure);
    condition
}
