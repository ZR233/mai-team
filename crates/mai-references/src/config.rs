//! 项目参考源绑定的校验、规范化与修订号协调。
//!
//! 规范化定义"配置是否相同"：条目按 `alias` 再按 `id` 排序后比较，因此仅调整排列
//! 顺序不会产生新修订；而 alias、说明、启用状态或源的变化都会产生新修订。
//! 唯一性检查覆盖禁用条目：禁用不等于可以重名，否则重新启用时会出现歧义授权。

use std::collections::{BTreeMap, BTreeSet};

use mai_protocol::{
    ALIAS_PATTERN, MAX_ALIAS_CHARS, MAX_JS_SAFE_INTEGER, ProjectReferenceBinding,
    REFERENCE_CONTAINER_ROOT, ReferenceErrorCode, ReferenceSource, ReplaceProjectReferencesRequest,
};
use uuid::Uuid;

use crate::error::{ReferenceError, Result};
use crate::policy::ReferenceDeploymentPolicy;
use crate::source::SourceAvailability;

/// 规范化后的绑定集合；已通过字段校验，且按 `(alias, id)` 排序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedBindings {
    bindings: Vec<ProjectReferenceBinding>,
}

impl NormalizedBindings {
    /// 规范化后的条目。
    pub fn bindings(&self) -> &[ProjectReferenceBinding] {
        &self.bindings
    }

    /// 取出条目。
    pub fn into_bindings(self) -> Vec<ProjectReferenceBinding> {
        self.bindings
    }

    /// 条目数量（含禁用条目）。
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// 是否没有任何条目。
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// 启用的条目。
    pub fn enabled(&self) -> Vec<&ProjectReferenceBinding> {
        self.bindings
            .iter()
            .filter(|binding| binding.enabled)
            .collect()
    }

    /// 与现有条目是否表达了同一个配置。
    pub fn is_equivalent_to(&self, existing: &[ProjectReferenceBinding]) -> bool {
        normalize_bindings(existing).bindings == self.bindings
    }

    /// 相同的规范化配置不产生新修订；否则返回下一个修订号。
    pub fn next_revision_for(
        &self,
        current_revision: u64,
        existing: &[ProjectReferenceBinding],
    ) -> Result<u64> {
        if self.is_equivalent_to(existing) {
            Ok(current_revision)
        } else {
            next_revision(current_revision)
        }
    }
}

/// 校验并规范化整体替换请求。
pub fn normalize_replace_request(
    policy: &ReferenceDeploymentPolicy,
    request: &ReplaceProjectReferencesRequest,
) -> Result<NormalizedBindings> {
    validate_bindings(policy, &request.bindings)?;
    Ok(normalize_bindings(&request.bindings))
}

/// 把条目排成规范顺序；不做校验，调用方必须先跑 [`validate_bindings`]。
pub fn normalize_bindings(bindings: &[ProjectReferenceBinding]) -> NormalizedBindings {
    let mut normalized = bindings.to_vec();
    normalized.sort_by(|left, right| {
        left.alias
            .cmp(&right.alias)
            .then_with(|| left.id.cmp(&right.id))
    });
    NormalizedBindings {
        bindings: normalized,
    }
}

/// 校验绑定字段与唯一性；禁用条目同样参与重复检查。
pub fn validate_bindings(
    policy: &ReferenceDeploymentPolicy,
    bindings: &[ProjectReferenceBinding],
) -> Result<()> {
    if bindings.len() > policy.max_bindings_per_project() as usize {
        return Err(ReferenceError::new(
            ReferenceErrorCode::TooManyBindings,
            format!(
                "单个项目最多绑定 {} 个参考源（含禁用条目）",
                policy.max_bindings_per_project()
            ),
        )
        .with_field("bindings"));
    }

    let mut ids: BTreeSet<Uuid> = BTreeSet::new();
    let mut aliases: BTreeSet<String> = BTreeSet::new();
    let mut source_ids: BTreeSet<Uuid> = BTreeSet::new();

    for (position, binding) in bindings.iter().enumerate() {
        validate_alias(&binding.alias)
            .map_err(|error| error.with_field(format!("bindings[{position}].alias")))?;
        validate_description(&binding.description, policy.max_description_chars())
            .map_err(|error| error.with_field(format!("bindings[{position}].description")))?;

        if !ids.insert(binding.id) {
            return Err(ReferenceError::new(
                ReferenceErrorCode::DuplicateBindingId,
                format!("条目 ID 重复：{}", binding.id),
            )
            .with_field(format!("bindings[{position}].id")));
        }
        if !aliases.insert(binding.alias.clone()) {
            return Err(ReferenceError::new(
                ReferenceErrorCode::DuplicateAlias,
                format!("alias 重复：{}", binding.alias),
            )
            .with_field(format!("bindings[{position}].alias")));
        }
        if !source_ids.insert(binding.source_id) {
            return Err(ReferenceError::new(
                ReferenceErrorCode::DuplicateSourceBinding,
                format!("同一参考源不能在同一项目重复绑定：{}", binding.source_id),
            )
            .with_field(format!("bindings[{position}].source_id")));
        }
    }
    Ok(())
}

/// 校验 alias 语法：小写字母开头，其余仅小写字母/数字/下划线/连字符，总长 ≤ 32。
pub fn validate_alias(alias: &str) -> Result<()> {
    let invalid = || {
        ReferenceError::new(
            ReferenceErrorCode::InvalidAlias,
            format!("alias 必须匹配 {ALIAS_PATTERN}：{alias}"),
        )
        .with_field("alias")
    };

    let mut characters = alias.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return Err(invalid()),
    }
    let rest: Vec<char> = characters.collect();
    if rest.len() > MAX_ALIAS_CHARS - 1 {
        return Err(invalid());
    }
    let allowed = rest.iter().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || *character == '_'
            || *character == '-'
    });
    if !allowed {
        return Err(invalid());
    }
    Ok(())
}

/// 校验说明字段：长度按 Unicode 字符数计算，禁止控制字符。
pub fn validate_description(description: &str, max_chars: usize) -> Result<()> {
    if description.chars().count() > max_chars {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidDescription,
            format!("参考源说明不能超过 {max_chars} 个字符"),
        )
        .with_field("description"));
    }
    if description
        .chars()
        .any(|character| (character as u32) < 0x20 || (character as u32) == 0x7f)
    {
        return Err(ReferenceError::new(
            ReferenceErrorCode::InvalidDescription,
            "参考源说明不能包含控制字符",
        )
        .with_field("description"));
    }
    Ok(())
}

/// 由 alias 生成容器内目标路径；alias 不合法时拒绝。
pub fn container_reference_path(alias: &str) -> Result<String> {
    validate_alias(alias)?;
    Ok(format!("{REFERENCE_CONTAINER_ROOT}/{alias}"))
}

/// 判断修订号是否是 JS 安全整数。
pub fn is_js_safe_revision(revision: u64) -> bool {
    revision <= MAX_JS_SAFE_INTEGER
}

/// 校验 CAS 的期望修订号。
pub fn check_expected_revision(expected: u64, current: u64) -> Result<()> {
    if !is_js_safe_revision(expected) {
        return Err(ReferenceError::new(
            ReferenceErrorCode::RevisionNotJsSafe,
            format!("请求中的 expected_revision 超出 JS 安全整数范围：{expected}"),
        )
        .with_field("expected_revision"));
    }
    if expected != current {
        return Err(ReferenceError::new(
            ReferenceErrorCode::ReferenceRevisionConflict,
            format!("参考源配置已变化：期望修订 {expected}，当前修订 {current}"),
        )
        .with_field("expected_revision"));
    }
    Ok(())
}

/// 计算下一个修订号；达到 JS 安全整数上限时显式失败，不溢出、不回绕。
pub fn next_revision(current: u64) -> Result<u64> {
    if !is_js_safe_revision(current) {
        return Err(ReferenceError::new(
            ReferenceErrorCode::RevisionNotJsSafe,
            format!("当前修订号超出 JS 安全整数范围：{current}"),
        ));
    }
    let next = current.checked_add(1).ok_or_else(revision_exhausted)?;
    if !is_js_safe_revision(next) {
        return Err(revision_exhausted());
    }
    Ok(next)
}

/// 逐条核对绑定引用的参考源记录与可用性。
///
/// 禁用条目也要求登记记录存在（注销被引用源时上层返回 409），但不要求源可用；
/// 启用条目必须是"验证通过且已确认可供 Agent/模型访问"。
pub fn validate_bound_sources(
    bindings: &[ProjectReferenceBinding],
    sources: &[ReferenceSource],
) -> Result<()> {
    let index: BTreeMap<Uuid, &ReferenceSource> =
        sources.iter().map(|source| (source.id, source)).collect();

    for (position, binding) in bindings.iter().enumerate() {
        let field = format!("bindings[{position}].source_id");
        let source = index.get(&binding.source_id).ok_or_else(|| {
            ReferenceError::new(
                ReferenceErrorCode::SourceNotFound,
                format!("绑定引用的参考源登记不存在：{}", binding.source_id),
            )
            .with_field(field.clone())
        })?;

        if !binding.enabled {
            continue;
        }

        match SourceAvailability::of(source) {
            SourceAvailability::Usable => {}
            SourceAvailability::NotApproved => {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::SourceNotApproved,
                    format!(
                        "参考源尚未确认可供 Agent/模型访问，不能启用：{}",
                        source.name
                    ),
                )
                .with_field(field));
            }
            availability => {
                return Err(ReferenceError::new(
                    ReferenceErrorCode::SourceNotReady,
                    format!(
                        "参考源{}，不能启用：{}",
                        availability.describe(),
                        source.name
                    ),
                )
                .with_field(field));
            }
        }
    }
    Ok(())
}

fn revision_exhausted() -> ReferenceError {
    ReferenceError::new(
        ReferenceErrorCode::RevisionExhausted,
        "参考源配置修订号已达到上限，无法继续递增",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use mai_protocol::MAX_DESCRIPTION_CHARS;
    use mai_protocol::{SourceProvenance, SourceState};
    use pretty_assertions::assert_eq;

    fn policy() -> ReferenceDeploymentPolicy {
        ReferenceDeploymentPolicy::new(vec![std::path::PathBuf::from("/srv/mai-references")])
            .expect("允许根")
    }

    fn sample_binding(alias: &str, enabled: bool) -> ProjectReferenceBinding {
        ProjectReferenceBinding {
            id: Uuid::new_v4(),
            source_id: Uuid::new_v4(),
            alias: alias.to_string(),
            description: "参考用途".to_string(),
            enabled,
        }
    }

    fn source(state: SourceState, approved: bool) -> ReferenceSource {
        ReferenceSource {
            id: Uuid::new_v4(),
            name: "Linux".to_string(),
            revision_label: "固定版本".to_string(),
            host_path: "/srv/mai-references/linux/repo".to_string(),
            provenance: SourceProvenance::directory_snapshot(),
            manifest_digest: Some("sha256:0".to_string()),
            state,
            approved_for_agent_access: approved,
            file_count: 1,
            byte_count: 1,
            source_device: 1,
            source_inode: 1,
            validation_error: None,
            warnings: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn alias_rules_are_enforced() {
        assert!(validate_alias("linux").is_ok());
        assert!(validate_alias("linux-6_12").is_ok());
        assert!(validate_alias("l").is_ok());
        assert!(validate_alias(&"a".repeat(MAX_ALIAS_CHARS)).is_ok());

        for invalid in [
            "",
            "Linux",
            "6linux",
            "-linux",
            "_linux",
            "linux.repo",
            "linux/repo",
            "linux repo",
        ] {
            assert_eq!(
                ReferenceErrorCode::InvalidAlias,
                validate_alias(invalid).expect_err("非法 alias").code(),
                "alias 应被拒绝：{invalid}"
            );
        }
        assert_eq!(
            ReferenceErrorCode::InvalidAlias,
            validate_alias(&"a".repeat(MAX_ALIAS_CHARS + 1))
                .expect_err("过长 alias")
                .code()
        );
    }

    #[test]
    fn container_path_is_derived_from_alias_only() {
        assert_eq!(
            "/references/linux",
            container_reference_path("linux").expect("目标路径")
        );
        assert_eq!(
            ReferenceErrorCode::InvalidAlias,
            container_reference_path("Linux")
                .expect_err("非法 alias")
                .code()
        );
    }

    #[test]
    fn duplicates_are_rejected_even_when_disabled() {
        let mut first = sample_binding("linux", true);
        let disabled = ProjectReferenceBinding {
            id: first.id,
            ..sample_binding("qemu", false)
        };
        assert_eq!(
            ReferenceErrorCode::DuplicateBindingId,
            validate_bindings(&policy(), &[first.clone(), disabled])
                .expect_err("重复条目 ID")
                .code()
        );

        let disabled = ProjectReferenceBinding {
            alias: "linux".to_string(),
            ..sample_binding("qemu", false)
        };
        assert_eq!(
            ReferenceErrorCode::DuplicateAlias,
            validate_bindings(&policy(), &[first.clone(), disabled])
                .expect_err("重复 alias")
                .code()
        );

        let disabled = ProjectReferenceBinding {
            source_id: first.source_id,
            ..sample_binding("qemu", false)
        };
        assert_eq!(
            ReferenceErrorCode::DuplicateSourceBinding,
            validate_bindings(&policy(), &[first.clone(), disabled])
                .expect_err("重复源")
                .code()
        );

        first.enabled = false;
        assert!(validate_bindings(&policy(), &[first]).is_ok());
    }

    #[test]
    fn too_many_bindings_are_rejected() {
        let policy = policy().with_max_bindings_per_project(1);
        let bindings = vec![sample_binding("linux", true), sample_binding("qemu", true)];
        assert_eq!(
            ReferenceErrorCode::TooManyBindings,
            validate_bindings(&policy, &bindings)
                .expect_err("超过上限")
                .code()
        );
    }

    #[test]
    fn descriptions_are_bounded_by_characters() {
        assert!(
            validate_description(&"说".repeat(MAX_DESCRIPTION_CHARS), MAX_DESCRIPTION_CHARS)
                .is_ok()
        );
        assert_eq!(
            ReferenceErrorCode::InvalidDescription,
            validate_description(
                &"说".repeat(MAX_DESCRIPTION_CHARS + 1),
                MAX_DESCRIPTION_CHARS
            )
            .expect_err("超长说明")
            .code()
        );
        assert_eq!(
            ReferenceErrorCode::InvalidDescription,
            validate_description("换\n行", MAX_DESCRIPTION_CHARS)
                .expect_err("控制字符")
                .code()
        );
    }

    #[test]
    fn normalization_ignores_order_but_not_content() {
        let linux = sample_binding("linux", true);
        let qemu = sample_binding("qemu", true);
        let normalized = normalize_bindings(&[qemu.clone(), linux.clone()]);
        assert_eq!(
            vec!["linux", "qemu"],
            normalized
                .bindings()
                .iter()
                .map(|entry| entry.alias.as_str())
                .collect::<Vec<_>>()
        );
        assert!(normalized.is_equivalent_to(&[linux.clone(), qemu.clone()]));

        let mut changed = qemu.clone();
        changed.description = "改过的用途".to_string();
        let existing = vec![linux.clone(), qemu.clone()];
        assert!(!normalized.is_equivalent_to(&[linux.clone(), changed.clone()]));
        assert_eq!(0, normalized.next_revision_for(0, &existing).expect("沿用"));

        let updated = normalize_bindings(&[linux, changed]);
        assert_eq!(2, updated.next_revision_for(1, &existing).expect("递增"));
    }

    #[test]
    fn revision_numbers_stay_js_safe() {
        assert!(is_js_safe_revision(MAX_JS_SAFE_INTEGER));
        assert!(!is_js_safe_revision(MAX_JS_SAFE_INTEGER + 1));
        assert_eq!(
            MAX_JS_SAFE_INTEGER,
            next_revision(MAX_JS_SAFE_INTEGER - 1).expect("递增到上限")
        );
        assert_eq!(
            ReferenceErrorCode::RevisionExhausted,
            next_revision(MAX_JS_SAFE_INTEGER)
                .expect_err("已达上限")
                .code()
        );
        assert_eq!(
            ReferenceErrorCode::RevisionNotJsSafe,
            next_revision(u64::MAX).expect_err("超出安全整数").code()
        );
        assert_eq!(
            ReferenceErrorCode::RevisionNotJsSafe,
            check_expected_revision(u64::MAX, u64::MAX)
                .expect_err("期望版本超范围")
                .code()
        );
    }

    #[test]
    fn expected_revision_must_match() {
        assert!(check_expected_revision(3, 3).is_ok());
        let error = check_expected_revision(3, 4).expect_err("CAS 冲突");
        assert_eq!(ReferenceErrorCode::ReferenceRevisionConflict, error.code());
        assert_eq!(409, error.http_status());
        assert!(error.field_error().is_some());
    }

    #[test]
    fn bound_sources_must_be_usable_when_enabled() {
        let enabled = source(SourceState::Ready, true);
        let binding = ProjectReferenceBinding {
            source_id: enabled.id,
            ..sample_binding("linux", true)
        };
        assert!(
            validate_bound_sources(
                std::slice::from_ref(&binding),
                std::slice::from_ref(&enabled)
            )
            .is_ok()
        );

        let unapproved = source(SourceState::Ready, false);
        let binding = ProjectReferenceBinding {
            source_id: unapproved.id,
            ..sample_binding("linux", true)
        };
        assert_eq!(
            ReferenceErrorCode::SourceNotApproved,
            validate_bound_sources(&[binding], &[unapproved])
                .expect_err("未确认")
                .code()
        );

        let validating = source(SourceState::Validating, true);
        let binding = ProjectReferenceBinding {
            source_id: validating.id,
            ..sample_binding("linux", true)
        };
        assert_eq!(
            ReferenceErrorCode::SourceNotReady,
            validate_bound_sources(&[binding], &[validating])
                .expect_err("验证中")
                .code()
        );
    }

    #[test]
    fn disabled_binding_tolerates_missing_source_state() {
        let missing = source(SourceState::Missing, true);
        let binding = ProjectReferenceBinding {
            source_id: missing.id,
            ..sample_binding("linux", false)
        };
        assert!(validate_bound_sources(&[binding], &[missing]).is_ok());

        let unknown = ProjectReferenceBinding {
            source_id: Uuid::new_v4(),
            ..sample_binding("qemu", false)
        };
        assert_eq!(
            ReferenceErrorCode::SourceNotFound,
            validate_bound_sources(&[unknown], &[])
                .expect_err("登记记录不存在")
                .code()
        );
    }
}
