use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const ANTHROPIC_SKILLS_REPO: &str = "https://github.com/anthropics/skills.git";
const ANTHROPIC_SKILLS_BRANCH: &str = "main";
const ANTHROPIC_SKILLS_SOURCE_DIR: &str = "skills";
const ANTHROPIC_SYSTEM_SKILLS_DIR: &str = "anthropic";

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let web_dir = manifest_dir.join("web");
    let system_skills_dir = manifest_dir.join("system-skills");
    let system_agents_dir = manifest_dir.join("system-agents");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let static_dir = out_dir.join("static");
    let embedded_system_skills_dir = out_dir.join("system-skills");
    let embedded_system_agents_dir = out_dir.join("system-agents");
    let anthropic_skills_repo_dir = out_dir.join("anthropic-skills");
    let staging_dir = out_dir.join("web-src");
    let npm_cache_dir = out_dir.join("npm-cache");
    let system_skills_refresh_stamp = out_dir.join("system-skills-refresh.stamp");

    force_system_skills_refresh(&system_skills_refresh_stamp);
    watch_dir(&system_skills_dir);
    prepare_system_skills_dir(
        &system_skills_dir,
        &embedded_system_skills_dir,
        &anthropic_skills_repo_dir,
    );
    watch_dir(&system_agents_dir);
    prepare_system_agents_dir(&system_agents_dir, &embedded_system_agents_dir);

    watch_frontend(&web_dir);
    prepare_staging_dir(&web_dir, &staging_dir);
    ensure_npm(&staging_dir, &npm_cache_dir);

    let static_arg = static_dir.to_string_lossy().to_string();
    run_npm(
        &staging_dir,
        &npm_cache_dir,
        [
            "run",
            "build",
            "--",
            "--outDir",
            static_arg.as_str(),
            "--emptyOutDir",
        ],
    );

    if !static_dir.join("index.html").exists() {
        panic!(
            "frontend build did not produce {}; expected npm run build to create embedded static output",
            static_dir.join("index.html").display()
        );
    }
}

fn force_system_skills_refresh(stamp: &Path) {
    println!("cargo:rerun-if-changed={}", stamp.display());
    if let Some(parent) = stamp.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|err| {
            panic!(
                "failed to create system skills refresh stamp dir {}: {err}",
                parent.display()
            )
        });
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    fs::write(stamp, now.to_string()).unwrap_or_else(|err| {
        panic!(
            "failed to write system skills refresh stamp {}: {err}",
            stamp.display()
        )
    });
}

fn prepare_system_skills_dir(source_dir: &Path, target_dir: &Path, anthropic_repo_dir: &Path) {
    if target_dir.exists() {
        fs::remove_dir_all(target_dir).unwrap_or_else(|err| {
            panic!(
                "failed to remove old system skills dir {}: {err}",
                target_dir.display()
            )
        });
    }
    if source_dir.exists() {
        copy_dir(source_dir, target_dir, should_skip_system_skill_entry);
    } else {
        fs::create_dir_all(target_dir).unwrap_or_else(|err| {
            panic!(
                "failed to create empty system skills dir {}: {err}",
                target_dir.display()
            )
        });
    }
    update_anthropic_skills_repo(anthropic_repo_dir);
    copy_anthropic_skills(anthropic_repo_dir, target_dir);
}

fn prepare_system_agents_dir(source_dir: &Path, target_dir: &Path) {
    if target_dir.exists() {
        fs::remove_dir_all(target_dir).unwrap_or_else(|err| {
            panic!(
                "failed to remove old system agents dir {}: {err}",
                target_dir.display()
            )
        });
    }
    if source_dir.exists() {
        copy_dir(source_dir, target_dir, should_skip_system_agent_entry);
    } else {
        fs::create_dir_all(target_dir).unwrap_or_else(|err| {
            panic!(
                "failed to create empty system agents dir {}: {err}",
                target_dir.display()
            )
        });
    }
}

fn update_anthropic_skills_repo(repo_dir: &Path) {
    if repo_dir.join(".git").is_dir() {
        run_git(
            repo_dir,
            [
                "fetch",
                "--depth",
                "1",
                "--filter=blob:none",
                "origin",
                ANTHROPIC_SKILLS_BRANCH,
            ],
        );
        run_git(
            repo_dir,
            [
                "checkout",
                "--force",
                &format!("origin/{ANTHROPIC_SKILLS_BRANCH}"),
            ],
        );
        run_git(
            repo_dir,
            ["sparse-checkout", "set", ANTHROPIC_SKILLS_SOURCE_DIR],
        );
        return;
    }

    if repo_dir.exists() {
        fs::remove_dir_all(repo_dir).unwrap_or_else(|err| {
            panic!(
                "failed to remove invalid anthropic skills repo dir {}: {err}",
                repo_dir.display()
            )
        });
    }
    let parent = repo_dir.parent().unwrap_or_else(|| {
        panic!(
            "anthropic skills repo dir {} has no parent",
            repo_dir.display()
        )
    });
    fs::create_dir_all(parent).unwrap_or_else(|err| {
        panic!(
            "failed to create anthropic skills repo parent {}: {err}",
            parent.display()
        )
    });
    run_git(
        parent,
        [
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            "--branch",
            ANTHROPIC_SKILLS_BRANCH,
            ANTHROPIC_SKILLS_REPO,
            repo_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_else(|| {
                    panic!(
                        "anthropic skills repo dir {} does not end in valid utf-8",
                        repo_dir.display()
                    )
                }),
        ],
    );
    run_git(
        repo_dir,
        ["sparse-checkout", "set", ANTHROPIC_SKILLS_SOURCE_DIR],
    );
}

fn run_git<const N: usize>(working_dir: &Path, args: [&str; N]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .status()
        .unwrap_or_else(|err| {
            panic!(
                "failed to execute git in {}; install git first: {err}",
                working_dir.display()
            )
        });
    if !status.success() {
        panic!(
            "git command failed in {} with status {status}",
            working_dir.display()
        );
    }
}

fn copy_anthropic_skills(repo_dir: &Path, target_dir: &Path) {
    let source_dir = repo_dir.join(ANTHROPIC_SKILLS_SOURCE_DIR);
    if !source_dir.is_dir() {
        panic!(
            "anthropic skills clone did not contain {}",
            source_dir.display()
        );
    }
    copy_dir(
        &source_dir,
        &target_dir.join(ANTHROPIC_SYSTEM_SKILLS_DIR),
        should_skip_anthropic_skill_entry,
    );
}

fn watch_frontend(web_dir: &Path) {
    for path in [
        web_dir.join("index.html"),
        web_dir.join("package.json"),
        web_dir.join("package-lock.json"),
        web_dir.join("vite.config.js"),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    watch_dir(&web_dir.join("src"));
    watch_dir(&web_dir.join("public"));
}

fn watch_dir(path: &Path) {
    if !path.exists() {
        return;
    }
    for entry in fs::read_dir(path).unwrap_or_else(|err| {
        panic!(
            "failed to read frontend directory {}: {err}",
            path.display()
        )
    }) {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read frontend entry: {err}"));
        let path = entry.path();
        if path.is_dir() {
            watch_dir(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn prepare_staging_dir(web_dir: &Path, staging_dir: &Path) {
    if staging_dir.exists() {
        fs::remove_dir_all(staging_dir).unwrap_or_else(|err| {
            panic!(
                "failed to remove old frontend staging dir {}: {err}",
                staging_dir.display()
            )
        });
    }
    copy_web_source(web_dir, staging_dir);
}

fn ensure_npm(web_dir: &Path, npm_cache_dir: &Path) {
    run_npm(web_dir, npm_cache_dir, ["--version"]);
    if frontend_dependencies_missing(web_dir, npm_cache_dir) {
        println!(
            "cargo:warning=frontend npm dependencies are missing or incomplete; installing them"
        );
        install_frontend_dependencies(web_dir, npm_cache_dir);
    }
}

fn frontend_dependencies_missing(web_dir: &Path, npm_cache_dir: &Path) -> bool {
    !web_dir.join("node_modules").is_dir()
        || !local_npm_bin_exists(web_dir, "vite")
        || !npm_command_succeeds(web_dir, npm_cache_dir, ["ls", "--depth=0", "--silent"])
}

fn local_npm_bin_exists(web_dir: &Path, name: &str) -> bool {
    let bin_dir = web_dir.join("node_modules").join(".bin");
    if cfg!(windows) {
        bin_dir.join(format!("{name}.cmd")).exists()
    } else {
        bin_dir.join(name).exists()
    }
}

fn install_frontend_dependencies(web_dir: &Path, npm_cache_dir: &Path) {
    if web_dir.join("package-lock.json").exists() {
        run_npm(web_dir, npm_cache_dir, ["ci"]);
    } else {
        run_npm(web_dir, npm_cache_dir, ["install"]);
    }
}

fn npm_command_succeeds<const N: usize>(
    web_dir: &Path,
    npm_cache_dir: &Path,
    args: [&str; N],
) -> bool {
    Command::new("npm")
        .args(args)
        .current_dir(web_dir)
        .env("npm_config_cache", npm_cache_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|err| {
            panic!(
                "failed to execute npm in {}; install Node.js/npm first: {err}",
                web_dir.display()
            )
        })
        .success()
}

fn run_npm<const N: usize>(web_dir: &Path, npm_cache_dir: &Path, args: [&str; N]) {
    let status = Command::new("npm")
        .args(args)
        .current_dir(web_dir)
        .env("npm_config_cache", npm_cache_dir)
        .status()
        .unwrap_or_else(|err| {
            panic!(
                "failed to execute npm in {}; install Node.js/npm first: {err}",
                web_dir.display()
            )
        });
    if !status.success() {
        panic!(
            "npm command failed in {} with status {status}",
            web_dir.display()
        );
    }
}

fn copy_web_source(from: &Path, to: &Path) {
    copy_dir(from, to, should_skip_web_source_entry);
}

fn copy_dir(from: &Path, to: &Path, should_skip: fn(&OsStr) -> bool) {
    fs::create_dir_all(to)
        .unwrap_or_else(|err| panic!("failed to create directory {}: {err}", to.display()));
    for entry in fs::read_dir(from)
        .unwrap_or_else(|err| panic!("failed to read directory {}: {err}", from.display()))
    {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read directory entry: {err}"));
        let source = entry.path();
        let file_name = entry.file_name();
        if should_skip(file_name.as_os_str()) {
            continue;
        }
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_dir(&source, &target, should_skip);
        } else {
            fs::copy(&source, &target).unwrap_or_else(|err| {
                panic!(
                    "failed to copy {} to {}: {err}",
                    source.display(),
                    target.display()
                )
            });
        }
    }
}

fn should_skip_web_source_entry(file_name: &OsStr) -> bool {
    matches!(
        file_name.to_str(),
        Some("node_modules" | "dist" | ".vite" | ".DS_Store")
    )
}

fn should_skip_system_skill_entry(file_name: &OsStr) -> bool {
    matches!(file_name.to_str(), Some(".DS_Store"))
}

fn should_skip_system_agent_entry(file_name: &OsStr) -> bool {
    matches!(file_name.to_str(), Some(".DS_Store"))
}

fn should_skip_anthropic_skill_entry(file_name: &OsStr) -> bool {
    let Some(name) = file_name.to_str() else {
        return false;
    };
    name == ".DS_Store" || name.starts_with('.')
}
