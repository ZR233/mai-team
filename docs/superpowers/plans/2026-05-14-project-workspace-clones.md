# Project Workspace Clones Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the smallest issue #9 closed loop: project repo cache at `repo.git`, per-agent clones under `clones/{agent_id}/repo`, clone-backed container mounts, clone cleanup, and Git tools that operate on clone paths.

**Architecture:** Split `crates/mai-runtime/src/projects/workspace.rs` into a focused directory module. `paths.rs` owns path construction, `git.rs` owns Git command execution and token redaction, and `manager.rs` owns repo cache and clone lifecycle. Existing callers keep simple compatibility wrappers while the runtime moves from worktree language to clone-backed workspace operations.

**Tech Stack:** Rust 2024, Tokio process execution, `tempfile`, `pretty_assertions`, existing `mai_protocol` IDs and `RuntimeError`.

---

## File Structure

- Create `crates/mai-runtime/src/projects/workspace/mod.rs`: public module boundary and compatibility exports.
- Create `crates/mai-runtime/src/projects/workspace/paths.rs`: project directory, `repo.git`, `clones`, `tmp`, and per-agent clone path helpers.
- Create `crates/mai-runtime/src/projects/workspace/git.rs`: `git_plain`, `git_with_token`, askpass setup, output redaction.
- Create `crates/mai-runtime/src/projects/workspace/manager.rs`: repo cache sync, agent clone prepare/cleanup, project workspace deletion.
- Modify `crates/mai-runtime/src/projects/mod.rs`: keep `projects::workspace` module path compiling after the file-to-directory split.
- Modify `crates/mai-runtime/src/lib.rs`: create project workspace with repo cache plus maintainer clone; mount clones into project agent containers; delete agents/projects clean clones/workspaces.
- Modify `crates/mai-runtime/src/tools/git.rs`: resolve `agent_clone_path` and return clone-oriented workspace info.
- Modify `crates/mai-tools/src/definitions/git.rs`: update tool descriptions from worktree to workspace/clone language and add `git_workspace_info` if needed in this phase.
- Modify focused tests near implementation modules first; update broader runtime integration tests only after the module-level red/green cycle is stable.

## Task 1: Workspace Path Model

**Files:**
- Create: `crates/mai-runtime/src/projects/workspace/mod.rs`
- Create: `crates/mai-runtime/src/projects/workspace/paths.rs`
- Move from: `crates/mai-runtime/src/projects/workspace.rs`

- [ ] **Step 1: Write the failing path test**

Move the existing path test next to the path code and change the expected layout:

```rust
#[test]
fn project_workspace_paths_use_repo_cache_and_agent_clones() {
    let root = PathBuf::from("/data/.mai-team/projects");
    let project_id = uuid::Uuid::nil();
    let agent_id = uuid::Uuid::nil();

    let paths = project_paths(&root, project_id);

    assert_eq!(
        paths,
        ProjectWorkspacePaths {
            project_dir: root.join(project_id.to_string()),
            repo_cache_path: root.join(project_id.to_string()).join("repo.git"),
            clones_dir: root.join(project_id.to_string()).join("clones"),
            tmp_dir: root.join(project_id.to_string()).join("tmp"),
        }
    );
    assert_eq!(
        agent_clone_path(&root, project_id, agent_id),
        root.join(project_id.to_string())
            .join("clones")
            .join(agent_id.to_string())
            .join("repo")
    );
}
```

- [ ] **Step 2: Verify the test fails**

Run: `cargo test -p mai-runtime projects::workspace::paths::tests::project_workspace_paths_use_repo_cache_and_agent_clones`

Expected: failure because `project_paths`, `ProjectWorkspacePaths`, and `agent_clone_path` do not exist yet.

- [ ] **Step 3: Implement path types and exports**

Create `paths.rs` with `PROJECT_REPO_CACHE_DIR`, `PROJECT_CLONES_DIR`, `PROJECT_TMP_DIR`, `ProjectWorkspacePaths`, `project_dir`, `project_paths`, `project_repo_cache_path`, `agent_clone_path`, and `project_tmp_path`. Export them from `workspace/mod.rs`.

- [ ] **Step 4: Verify the path test passes**

Run: `cargo test -p mai-runtime projects::workspace::paths::tests::project_workspace_paths_use_repo_cache_and_agent_clones`

Expected: pass.

## Task 2: Repo Cache and Clone Lifecycle

**Files:**
- Create: `crates/mai-runtime/src/projects/workspace/manager.rs`
- Create: `crates/mai-runtime/src/projects/workspace/git.rs`
- Modify: `crates/mai-runtime/src/projects/workspace/mod.rs`

- [ ] **Step 1: Write failing tests for Git command shape**

Use a temporary fake `git` script in module tests to capture commands. Cover:

```text
sync_project_repo_cache creates or updates projects/{project_id}/repo.git
prepare_project_agent_clone creates clones/{agent_id}/repo from repo.git
cleanup_project_agent_clone removes only that agent clone
delete_project_workspace removes repo.git, clones, and tmp through the project directory
```

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test -p mai-runtime projects::workspace::manager`

Expected: failures because clone/cache functions are not implemented.

- [ ] **Step 3: Implement minimal cache and clone functions**

Implement:

```rust
pub(crate) async fn sync_project_repo_cache(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    token: &str,
) -> Result<()>;

pub(crate) async fn prepare_project_agent_clone(
    git_binary: &str,
    projects_root: &Path,
    project: &ProjectSummary,
    agent_id: AgentId,
) -> Result<PathBuf>;

pub(crate) async fn cleanup_project_agent_clone(
    projects_root: &Path,
    project_id: ProjectId,
    agent_id: AgentId,
) -> Result<()>;
```

Keep old wrappers only as internal compatibility shims that call the new clone functions while callers are migrated.

- [ ] **Step 4: Verify manager tests pass**

Run: `cargo test -p mai-runtime projects::workspace::manager`

Expected: pass.

## Task 3: Runtime Container Mount Migration

**Files:**
- Modify: `crates/mai-runtime/src/lib.rs`
- Modify: `crates/mai-runtime/src/agents/container.rs`
- Modify: `crates/mai-runtime/src/tests/mod.rs`

- [ ] **Step 1: Write failing integration expectations**

Update the existing project workspace integration test so the fake git log and docker log require:

```text
clone --mirror -- https://github.com/owner/repo.git .../repo.git
clone --local --no-checkout .../repo.git .../clones/{agent_id}/repo
remote set-url origin https://github.com/owner/repo.git
checkout -B mai-agent/{agent_id} origin/main
.../clones/{agent_id}/repo:/workspace/repo
```

- [ ] **Step 2: Verify the test fails**

Run the specific updated test from `crates/mai-runtime/src/tests/mod.rs`.

Expected: failure showing old `clone --branch` and `worktree add` behavior.

- [ ] **Step 3: Migrate project startup and container mount**

Change project startup to call `sync_project_repo_cache` and `prepare_project_agent_clone`, then pass the clone path as `repo_mount` for project agents.

- [ ] **Step 4: Verify the integration test passes**

Run the same focused integration test.

Expected: pass.

## Task 4: Agent and Project Cleanup

**Files:**
- Modify: `crates/mai-runtime/src/agents/delete.rs`
- Modify: `crates/mai-runtime/src/lib.rs`
- Modify: `crates/mai-runtime/src/tests/mod.rs`

- [ ] **Step 1: Write failing cleanup tests**

Update deletion tests to create `projects/{project_id}/clones/{agent_id}/repo` and assert it is removed when an agent is deleted. Keep project deletion asserting the whole project directory is gone.

- [ ] **Step 2: Verify tests fail**

Run the focused deletion tests.

Expected: failure because deletion still calls review/worktree cleanup paths.

- [ ] **Step 3: Route deletion through clone cleanup**

For any agent with `project_id.is_some()`, call `cleanup_project_agent_clone(project_id, agent_id)` during agent deletion. Keep review-specific cleanup only for old review workspace paths still in use outside this milestone.

- [ ] **Step 4: Verify cleanup tests pass**

Run the focused deletion tests.

Expected: pass.

## Task 5: Git Tool Clone Path

**Files:**
- Modify: `crates/mai-runtime/src/tools/git.rs`
- Modify: `crates/mai-tools/src/definitions/git.rs`
- Modify: `crates/mai-tools/src/names.rs` if adding `git_workspace_info`

- [ ] **Step 1: Write failing tool tests**

Add or update tests so project Git tools look for `clones/{agent_id}/repo`, not `worktrees/{agent_id}`, and `git_worktree_info` returns clone-oriented fields for compatibility.

- [ ] **Step 2: Verify tests fail**

Run the focused Git tool tests.

Expected: failure because tools still call `agent_worktree_path`.

- [ ] **Step 3: Switch Git tools to clone paths**

Use `agent_clone_path` for tool execution. Keep schemas free of token, env, cwd, repo path, and workspace path arguments.

- [ ] **Step 4: Verify tool tests pass**

Run the focused Git tool tests and `cargo test -p mai-tools`.

Expected: pass.

## Task 6: Validation and Push

**Files:**
- All touched Rust files

- [ ] **Step 1: Format**

Run: `cargo fmt`

Expected: no formatting diff remains.

- [ ] **Step 2: Run focused runtime checks**

Run: `cargo test -p mai-runtime projects::workspace`

Expected: pass.

- [ ] **Step 3: Run tool definition checks**

Run: `cargo test -p mai-tools`

Expected: pass.

- [ ] **Step 4: Commit and push**

Commit message: `refactor: use clone-backed project workspaces`

Push branch: `refactor/issue-9-workspace-clones`

Expected: draft PR linked to issue #9 has the implementation commits.

## Spec Coverage Notes

This plan intentionally covers the issue #9 minimal first version and the first three milestones partially. It does not complete auto review migration, startup reconcile, old worktree migration, full GitPolicy hardening, or future API exposure; those remain follow-up PRs after the clone-backed project-agent loop is structurally closed.
