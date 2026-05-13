use std::collections::BTreeSet;
use std::fs;

use mai_protocol::{SkillConfigEntry, SkillScope, SkillsConfigRequest};
use tempfile::tempdir;

use crate::constants::SKILL_FILE;
use crate::paths::canonicalize_or_clone;
use crate::{SkillInput, SkillsManager, extract_skill_mentions};

#[test]
fn discovers_skill_frontmatter_and_metadata() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("demo");
    fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
    fs::create_dir_all(skill_dir.join("assets")).expect("mkdir assets");
    fs::write(skill_dir.join("assets/icon.png"), "icon").expect("icon");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: demo-skill\ndescription: Helps demo things.\nmetadata:\n  short-description: Short demo\n---\nbody",
    )
    .expect("write skill");
    fs::write(
        skill_dir.join("agents/openai.yaml"),
        "interface:\n  display_name: Demo\n  icon_small: assets/icon.png\n  default_prompt: Use demo\npolicy:\n  allow_implicit_invocation: false\ndependencies:\n  tools:\n    - type: mcp\n      value: filesystem\n",
    )
    .expect("write metadata");

    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
    let response = manager.list(&SkillsConfigRequest::default()).expect("list");
    assert!(response.errors.is_empty());
    assert_eq!(response.skills[0].name, "demo-skill");
    assert_eq!(
        response.skills[0].short_description.as_deref(),
        Some("Short demo")
    );
    assert_eq!(
        response.skills[0]
            .interface
            .as_ref()
            .and_then(|interface| interface.display_name.as_deref()),
        Some("Demo")
    );
    assert_eq!(
        response.skills[0]
            .dependencies
            .as_ref()
            .expect("dependencies")
            .tools[0]
            .kind,
        "mcp"
    );
    assert_eq!(
        response.skills[0]
            .policy
            .as_ref()
            .and_then(|policy| policy.allow_implicit_invocation),
        Some(false)
    );
}

#[test]
fn invalid_skill_reports_error() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("bad");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(skill_dir.join(SKILL_FILE), "body").expect("write");

    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
    let response = manager.list(&SkillsConfigRequest::default()).expect("list");
    assert!(response.skills.is_empty());
    assert_eq!(response.errors.len(), 1);
}

#[test]
fn discovers_skill_with_long_description() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("anthropic-like");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    let description = "Long description sentence. ".repeat(120);
    fs::write(
        skill_dir.join(SKILL_FILE),
        format!("---\nname: anthropic-like\ndescription: {description}\n---\nbody"),
    )
    .expect("write skill");

    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::System)]);
    let response = manager.list(&SkillsConfigRequest::default()).expect("list");

    assert!(response.errors.is_empty());
    assert_eq!(response.skills.len(), 1);
    assert_eq!(response.skills[0].description, description.trim());
}

#[test]
fn icon_paths_must_be_under_assets() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("demo");
    fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: demo\ndescription: Demo.\n---\nbody",
    )
    .expect("write skill");
    fs::write(
        skill_dir.join("agents/openai.yaml"),
        "interface:\n  icon_small: ../secret.png\n  icon_large: assets/icon.png\n",
    )
    .expect("write metadata");

    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
    let response = manager.list(&SkillsConfigRequest::default()).expect("list");
    let interface = response.skills[0].interface.as_ref().expect("interface");
    assert!(interface.icon_small.is_none());
    assert!(interface.icon_large.is_some());
}

#[test]
fn root_order_and_dedupe_prefers_repo() {
    let dir = tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let project = dir.path().join("project");
    let user = dir.path().join("user");
    let system = dir.path().join("system");
    for root in [&repo, &project, &user, &system] {
        let skill_dir = root.join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo.\n---\nbody",
        )
        .expect("write");
    }
    let manager = SkillsManager::with_roots(vec![
        (system.clone(), SkillScope::System),
        (user, SkillScope::User),
        (repo.clone(), SkillScope::Repo),
        (project.clone(), SkillScope::Project),
    ]);
    let response = manager.list(&SkillsConfigRequest::default()).expect("list");
    assert_eq!(response.skills[0].scope, SkillScope::Project);
    assert!(response.skills[0].path.starts_with(project));
    assert_eq!(response.skills[1].scope, SkillScope::Repo);
    assert!(response.skills[1].path.starts_with(repo));
    assert_eq!(response.skills[2].scope, SkillScope::User);
    assert_eq!(response.skills[3].scope, SkillScope::System);
    assert!(response.skills[3].path.starts_with(system));
}

#[test]
fn new_with_system_root_discovers_system_skills() {
    let dir = tempdir().expect("tempdir");
    let system = dir.path().join("system");
    let skill_dir = system.join("demo");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: system-demo\ndescription: System demo.\n---\nbody",
    )
    .expect("write");

    let manager = SkillsManager::new_with_system_root(dir.path(), Some(&system));
    let response = manager.list(&SkillsConfigRequest::default()).expect("list");

    let skill = response
        .skills
        .iter()
        .find(|skill| skill.name == "system-demo")
        .expect("system skill");
    assert_eq!(skill.scope, SkillScope::System);
    assert!(response.roots.iter().any(|root| root == &system));
}

#[test]
fn new_with_system_root_and_extra_roots_discovers_project_skills() {
    let dir = tempdir().expect("tempdir");
    let system = dir.path().join("system");
    let project = dir.path().join("project");
    for (root, name) in [(&system, "system-demo"), (&project, "project-demo")] {
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: {name}\n---\nbody"),
        )
        .expect("write");
    }

    let manager = SkillsManager::new_with_system_root_and_extra_roots(
        dir.path(),
        Some(&system),
        vec![(project.clone(), SkillScope::Project)],
    );
    let response = manager.list(&SkillsConfigRequest::default()).expect("list");

    let project_skill = response
        .skills
        .iter()
        .find(|skill| skill.name == "project-demo")
        .expect("project skill");
    assert_eq!(project_skill.scope, SkillScope::Project);
    assert!(project_skill.path.starts_with(project));
    assert!(
        response
            .skills
            .iter()
            .any(|skill| skill.name == "system-demo")
    );
}

#[test]
fn config_disables_by_name_and_path() {
    let dir = tempdir().expect("tempdir");
    for name in ["one", "two"] {
        let skill_dir = dir.path().join(name);
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: {name}\n---\nbody"),
        )
        .expect("write");
    }
    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
    let one_path = canonicalize_or_clone(dir.path().join("one/SKILL.md"));
    let response = manager
        .list(&SkillsConfigRequest {
            config: vec![
                SkillConfigEntry {
                    name: Some("two".to_string()),
                    path: None,
                    enabled: false,
                },
                SkillConfigEntry {
                    name: None,
                    path: Some(one_path),
                    enabled: false,
                },
            ],
        })
        .expect("list");
    assert!(response.skills.iter().all(|skill| !skill.enabled));
}

#[test]
fn config_disables_system_skill_by_name_and_path() {
    let dir = tempdir().expect("tempdir");
    let system = dir.path().join("system");
    for name in ["one", "two"] {
        let skill_dir = system.join(name);
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: {name}\n---\nbody"),
        )
        .expect("write");
    }
    let manager = SkillsManager::with_roots(vec![(system.clone(), SkillScope::System)]);
    let one_path = canonicalize_or_clone(system.join("one/SKILL.md"));
    let response = manager
        .list(&SkillsConfigRequest {
            config: vec![
                SkillConfigEntry {
                    name: Some("two".to_string()),
                    path: None,
                    enabled: false,
                },
                SkillConfigEntry {
                    name: None,
                    path: Some(one_path),
                    enabled: false,
                },
            ],
        })
        .expect("list");
    assert!(response.skills.iter().all(|skill| !skill.enabled));
}

#[test]
fn global_config_does_not_disable_project_scoped_skills() {
    let dir = tempdir().expect("tempdir");
    let project = dir.path().join("project");
    let skill_dir = project.join("demo");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: demo\ndescription: Demo.\n---\nbody",
    )
    .expect("write");
    let manager = SkillsManager::with_roots(vec![(project, SkillScope::Project)]);
    let response = manager
        .list(&SkillsConfigRequest {
            config: vec![SkillConfigEntry {
                name: Some("demo".to_string()),
                path: None,
                enabled: false,
            }],
        })
        .expect("list");

    assert!(response.skills[0].enabled);
}

#[test]
fn explicit_path_beats_duplicate_plain_name() {
    let dir = tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let user = dir.path().join("user");
    for root in [&repo, &user] {
        let skill_dir = root.join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo.\n---\nbody",
        )
        .expect("write");
    }
    let manager = SkillsManager::with_roots(vec![
        (repo, SkillScope::Repo),
        (user.clone(), SkillScope::User),
    ]);
    let ambiguous = manager
        .build_injections(&["demo".to_string()], &SkillsConfigRequest::default())
        .expect("inject");
    assert!(ambiguous.items.is_empty());
    let user_path = canonicalize_or_clone(user.join("demo/SKILL.md"));
    let selected = manager
        .build_injections(
            &[user_path.display().to_string(), "demo".to_string()],
            &SkillsConfigRequest::default(),
        )
        .expect("inject");
    assert_eq!(selected.items.len(), 1);
    assert_eq!(selected.items[0].metadata.scope, SkillScope::User);
}

#[test]
fn explicit_mentions_do_not_fuzzy_match_display_plugin_directory_or_abbreviation() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("doc-reviewer");
    fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: github:doc-review\ndescription: Review docs.\n---\nBody.",
    )
    .expect("write skill");
    fs::write(
        skill_dir.join("agents/openai.yaml"),
        "interface:\n  display_name: Documentation Review\n",
    )
    .expect("write metadata");
    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

    for mention in ["Documentation Review", "doc-reviewer", "dr"] {
        let injected = manager
            .build_injections(&[mention.to_string()], &SkillsConfigRequest::default())
            .expect("inject");
        assert!(injected.items.is_empty(), "mention {mention}");
    }
}

#[test]
fn text_skill_mentions_require_exact_boundary() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("alpha-skill");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: alpha-skill\ndescription: Alpha.\n---\nBody.",
    )
    .expect("write skill");
    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

    let near_miss = manager
        .build_injections_for_message("$alpha-skillx", &[], &SkillsConfigRequest::default())
        .expect("near miss");
    assert!(near_miss.items.is_empty());

    let matched = manager
        .build_injections_for_message("$alpha-skill.", &[], &SkillsConfigRequest::default())
        .expect("match");
    assert_eq!(matched.items.len(), 1);
    assert_eq!(matched.items[0].metadata.name, "alpha-skill");
}

#[test]
fn plain_text_skill_mentions_require_exact_boundary() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("alpha-skill");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: alpha-skill\ndescription: Alpha.\n---\nBody.",
    )
    .expect("write skill");
    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

    let near_miss = manager
        .build_injections_for_message(
            "please use alpha-skillx",
            &[],
            &SkillsConfigRequest::default(),
        )
        .expect("near miss");
    assert!(near_miss.items.is_empty());

    let matched = manager
        .build_injections_for_message(
            "please use alpha-skill, thanks",
            &[],
            &SkillsConfigRequest::default(),
        )
        .expect("match");
    assert_eq!(matched.items.len(), 1);
    assert_eq!(matched.items[0].metadata.name, "alpha-skill");
}

#[test]
fn linked_path_mentions_select_by_path_and_block_plain_fallback() {
    let dir = tempdir().expect("tempdir");
    let alpha = dir.path().join("alpha");
    let beta = dir.path().join("beta");
    for root in [&alpha, &beta] {
        let skill_dir = root.join("demo");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo.\n---\nBody.",
        )
        .expect("write skill");
    }
    let manager = SkillsManager::with_roots(vec![
        (alpha, SkillScope::Repo),
        (beta.clone(), SkillScope::User),
    ]);
    let beta_path = canonicalize_or_clone(beta.join("demo/SKILL.md"));

    let injected = manager
        .build_injections_for_message(
            &format!("use [$demo]({}) and $demo", beta_path.display()),
            &[],
            &SkillsConfigRequest::default(),
        )
        .expect("inject");
    assert_eq!(injected.items.len(), 1);
    assert_eq!(injected.items[0].metadata.scope, SkillScope::User);

    let missing = manager
        .build_injections_for_message(
            "use [$demo](/tmp/missing/SKILL.md) and $demo",
            &[],
            &SkillsConfigRequest::default(),
        )
        .expect("missing");
    assert!(missing.items.is_empty());
}

#[test]
fn reserved_name_conflict_blocks_plain_name_but_not_path() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("demo");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: demo\ndescription: Demo.\n---\nBody.",
    )
    .expect("write skill");
    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);
    let reserved_names = BTreeSet::from(["demo".to_string()]);

    let blocked = manager
        .build_injections_for_input(
            SkillInput {
                text: Some("$demo"),
                reserved_names: reserved_names.clone(),
                ..Default::default()
            },
            &SkillsConfigRequest::default(),
        )
        .expect("blocked");
    assert!(blocked.items.is_empty());

    let plain_blocked = manager
        .build_injections_for_input(
            SkillInput {
                text: Some("please use demo"),
                reserved_names: reserved_names.clone(),
                ..Default::default()
            },
            &SkillsConfigRequest::default(),
        )
        .expect("plain blocked");
    assert!(plain_blocked.items.is_empty());

    let path = canonicalize_or_clone(skill_dir.join(SKILL_FILE));
    let selected = manager
        .build_injections_for_input(
            SkillInput {
                text: Some(&format!("use [$demo]({})", path.display())),
                reserved_names,
                ..Default::default()
            },
            &SkillsConfigRequest::default(),
        )
        .expect("path");
    assert_eq!(selected.items.len(), 1);
}

#[test]
fn implicit_policy_hides_from_available_but_explicit_can_inject() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("guarded");
    fs::create_dir_all(skill_dir.join("agents")).expect("mkdir metadata");
    fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: guarded-skill\ndescription: Guarded skill.\n---\nGuarded body.",
    )
    .expect("write skill");
    fs::write(
        skill_dir.join("agents/openai.yaml"),
        "policy:\n  allow_implicit_invocation: false\n",
    )
    .expect("write metadata");
    let manager = SkillsManager::with_roots(vec![(dir.path().to_path_buf(), SkillScope::Repo)]);

    let available = manager
        .render_available(&SkillsConfigRequest::default())
        .expect("available");
    assert_eq!(available, "No skills are currently available.");

    let explicit = manager
        .build_injections_for_message(
            "please help",
            &["guarded-skill".to_string()],
            &SkillsConfigRequest::default(),
        )
        .expect("explicit");
    assert_eq!(explicit.items.len(), 1);

    let plain = manager
        .build_injections_for_message(
            "please use guarded-skill",
            &[],
            &SkillsConfigRequest::default(),
        )
        .expect("plain");
    assert!(plain.items.is_empty());

    let explicit_text = manager
        .build_injections_for_message(
            "please use $guarded-skill",
            &[],
            &SkillsConfigRequest::default(),
        )
        .expect("explicit text");
    assert_eq!(explicit_text.items.len(), 1);
}

#[test]
fn extracts_skill_mentions_with_boundaries_and_env_skip() {
    assert_eq!(
        extract_skill_mentions("use $rust-dev, $plugin:skill and $PATH then $HOME"),
        vec!["rust-dev", "plugin:skill"]
    );
    assert_eq!(
        extract_skill_mentions("$alpha-skillx and $alpha-skill"),
        vec!["alpha-skillx", "alpha-skill"]
    );
}
