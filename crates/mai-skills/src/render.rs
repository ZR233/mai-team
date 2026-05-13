use std::path::Path;

use mai_protocol::{SkillMetadata, SkillsListResponse};

use crate::ordering::skill_sort;

pub fn render_available_response(response: SkillsListResponse) -> String {
    let mut skills = response
        .skills
        .into_iter()
        .filter(|skill| {
            skill.enabled
                && skill
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.allow_implicit_invocation)
                    .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    if skills.is_empty() {
        return "No skills are currently available.".to_string();
    }

    skills.sort_by(skill_sort);

    let mut lines = Vec::with_capacity(skills.len() + 7);
    lines.push("A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and file path so you can open the source for full instructions when using a specific skill.".to_string());
    lines.push("### Available Skills".to_string());
    for skill in skills {
        lines.push(format!(
            "- ${}: {} (path: {})",
            skill.name,
            display_description(&skill),
            skill_display_path(&skill).display()
        ));
    }
    lines.push("### How to use skills".to_string());
    lines.push("- Discovery: The list above is the skills available in this session (name + description + file path). Skill bodies live on disk at the listed paths.".to_string());
    lines.push("- Trigger rules: If the user names a skill (with `$SkillName` or plain text), selects it from the UI, or the task clearly matches a skill's description shown above, you must use that skill for that turn. Multiple mentions mean use them all. Do not carry skills across turns unless re-mentioned.".to_string());
    lines.push("- Missing/blocked: If a named skill is not in the list or the path cannot be read, say so briefly and continue with the best fallback.".to_string());
    lines.push("- How to use a skill: after deciding to use a skill, open its `SKILL.md`. Read only enough to follow the workflow. Resolve relative references from the directory containing that `SKILL.md`; if it points to `references/`, `scripts/`, or `assets/`, load only the specific files needed.".to_string());
    lines.push("- Coordination and context hygiene: if multiple skills apply, choose the minimal set that covers the request and state the order you will use them. Keep context small and avoid deep reference-chasing unless blocked.".to_string());
    lines.join("\n")
}

fn display_description(skill: &SkillMetadata) -> &str {
    skill
        .short_description
        .as_deref()
        .or_else(|| {
            skill
                .interface
                .as_ref()
                .and_then(|interface| interface.short_description.as_deref())
        })
        .unwrap_or(&skill.description)
}

fn skill_display_path(skill: &SkillMetadata) -> &Path {
    skill.source_path.as_deref().unwrap_or(&skill.path)
}
