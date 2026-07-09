use mai_protocol::{SkillMetadata, SkillScope};

pub(crate) fn skill_sort(left: &SkillMetadata, right: &SkillMetadata) -> std::cmp::Ordering {
    scope_rank(left.scope)
        .cmp(&scope_rank(right.scope))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.path.cmp(&right.path))
}

fn scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::Project => 0,
        SkillScope::Repo => 1,
        SkillScope::User => 2,
        SkillScope::System => 3,
    }
}
