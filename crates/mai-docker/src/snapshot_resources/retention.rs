//! 回收时点的保护集合。
//!
//! 调用方（运行时）比本层更清楚哪些快照镜像还在途：未终态或 Failed 的收据、尚未发布
//! binding 的 clone 出生 Agent、以及刚提交但元数据还没落盘的镜像。这些保护必须在调用
//! 回收前显式给出；本层不认识数据库，也不做任何“猜归属”的兜底。

use std::collections::BTreeSet;

use uuid::Uuid;

/// 一次回收流程中必须保留的快照镜像来源。
///
/// 三个集合互相独立：镜像 ID 覆盖“已知道具体镜像”的在途操作，operation 覆盖“收据里
/// 还没写镜像 ID”的在途操作，clone Agent 覆盖“子 Agent 还没发布 binding”的出生镜像。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotRetention {
    protected_image_ids: BTreeSet<String>,
    protected_operation_ids: BTreeSet<Uuid>,
    protected_clone_agent_ids: BTreeSet<String>,
}

impl SnapshotRetention {
    /// 空保护集合：只有调用方确认“没有任何在途操作”时才可以直接使用。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加受保护的镜像 ID；ID 可以带或不带 `sha256:` 前缀，比较前统一归一化。
    pub fn with_image_ids<T, I>(mut self, image_ids: I) -> Self
    where
        T: Into<String>,
        I: IntoIterator<Item = T>,
    {
        self.protected_image_ids.extend(
            image_ids
                .into_iter()
                .map(|image_id| normalize_image_id(&image_id.into())),
        );
        self
    }

    /// 追加受保护的 operation：未终态或 Failed 的收据，即使镜像 ID 还没落盘。
    pub fn with_operation_ids<I>(mut self, operation_ids: I) -> Self
    where
        I: IntoIterator<Item = Uuid>,
    {
        self.protected_operation_ids.extend(operation_ids);
        self
    }

    /// 追加“clone 出生且尚未发布 binding”的 Agent ID。
    pub fn with_clone_agent_ids<T, I>(mut self, agent_ids: I) -> Self
    where
        T: Into<String>,
        I: IntoIterator<Item = T>,
    {
        self.protected_clone_agent_ids.extend(
            agent_ids
                .into_iter()
                .map(|agent_id| normalize_agent_id(&agent_id.into())),
        );
        self
    }

    /// 受保护的镜像 ID（已归一化）。
    pub fn protected_image_ids(&self) -> &BTreeSet<String> {
        &self.protected_image_ids
    }

    /// 受保护的 operation ID。
    pub fn protected_operation_ids(&self) -> &BTreeSet<Uuid> {
        &self.protected_operation_ids
    }

    /// 受保护的 clone 出生 Agent ID。
    pub fn protected_clone_agent_ids(&self) -> &BTreeSet<String> {
        &self.protected_clone_agent_ids
    }

    /// 指定镜像 ID 是否被显式保护。
    pub(crate) fn protects_image(&self, image_id: &str) -> bool {
        self.protected_image_ids
            .contains(&normalize_image_id(image_id))
    }

    /// 指定 operation 是否被显式保护。
    pub(crate) fn protects_operation(&self, operation_id: Uuid) -> bool {
        self.protected_operation_ids.contains(&operation_id)
    }

    /// 指定 Agent 的 clone 出生镜像是否被显式保护。
    pub(crate) fn protects_clone_agent(&self, agent_id: &str) -> bool {
        self.protected_clone_agent_ids
            .contains(&normalize_agent_id(agent_id))
    }
}

/// 归一化镜像 ID：去掉首尾空白、转小写、去掉 `sha256:` 前缀。
///
/// Docker 在不同命令里返回 `sha256:ABC…` 形式的 ID，而收据与 binding 可能只保存长 ID
/// 或大小写不同的形式；两侧比较前都必须走这里，否则保护会静默失效。
pub(crate) fn normalize_image_id(image_id: &str) -> String {
    let normalized = image_id.trim().to_ascii_lowercase();
    match normalized.strip_prefix("sha256:") {
        Some(digest) => digest.to_string(),
        None => normalized,
    }
}

/// 归一化 Agent 标识：能解析成 UUID 的统一成规范的连字符小写形式。
///
/// 标签里的 Agent 标识与调用方（binding/收据）给出的取值可能大小写或书写形式不同，
/// 两侧必须走同一套归一化，否则“未发布 binding 的 clone 出生镜像”会静默漏保护。
/// 解析不成的取值按去除首尾空白后的原样保留：归属核对只接受 UUID 形式的 Agent 标签，
/// 这类取值不会参与回收判定，但调用方给出的保护仍然要留着。
pub(crate) fn normalize_agent_id(agent_id: &str) -> String {
    let trimmed = agent_id.trim();
    match Uuid::parse_str(trimmed) {
        Ok(uuid) => uuid.to_string(),
        Err(_) => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    use super::{SnapshotRetention, normalize_image_id};

    #[test]
    fn normalizes_image_ids_regardless_of_prefix_case_and_padding() {
        assert_eq!(
            normalize_image_id("  sha256:ABC123  "),
            "abc123".to_string()
        );
        assert_eq!(normalize_image_id("abc123"), "abc123".to_string());
        assert_eq!(normalize_image_id("sha256:"), "".to_string());
    }

    #[test]
    fn normalizes_clone_agent_ids_so_protection_cannot_silently_miss() {
        let retention = SnapshotRetention::new()
            .with_clone_agent_ids(["  ABCDEF01-2345-6789-ABCD-EF0123456789 "]);

        assert_eq!(
            retention
                .protected_clone_agent_ids()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["abcdef01-2345-6789-abcd-ef0123456789".to_string()]
        );
        assert!(retention.protects_clone_agent("abcdef01-2345-6789-abcd-ef0123456789"));
        assert!(retention.protects_clone_agent("ABCDEF0123456789ABCDEF0123456789"));
        assert!(!retention.protects_clone_agent("11111111-1111-1111-1111-111111111111"));
    }

    #[test]
    fn applies_every_protection_set() {
        let operation = Uuid::from_u128(7);
        let retention = SnapshotRetention::new()
            .with_image_ids(["SHA256:ABC123", "def456"])
            .with_operation_ids([operation])
            .with_clone_agent_ids(["child-agent"]);

        assert_eq!(
            retention
                .protected_image_ids()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["abc123".to_string(), "def456".to_string()]
        );
        assert_eq!(
            retention
                .protected_operation_ids()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![operation]
        );
        assert_eq!(
            retention
                .protected_clone_agent_ids()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["child-agent".to_string()]
        );
        assert!(retention.protects_image("sha256:abc123"));
        assert!(retention.protects_operation(operation));
        assert!(retention.protects_clone_agent("child-agent"));
        assert!(!retention.protects_image("sha256:other"));
        assert!(!retention.protects_clone_agent("other-agent"));
    }
}
