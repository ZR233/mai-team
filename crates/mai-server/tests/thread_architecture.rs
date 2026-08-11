use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "SessionEvent",
    "SessionStreamFrame",
    "runtime_agent_id",
    "session_wire",
    "/sessions/",
];

#[test]
fn production_and_web_have_only_the_thread_protocol() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let roots = [
        workspace.join("crates/mai-protocol/src"),
        workspace.join("crates/mai-runtime/src"),
        workspace.join("crates/mai-server/src"),
        workspace.join("crates/mai-store/src"),
        workspace.join("crates/mai-server/web/src"),
        workspace.join("crates/mai-server/web/e2e"),
        workspace.join("crates/mai-server/web/vite.config.ts"),
    ];
    let mut violations = Vec::new();
    for root in roots {
        scan(&root, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "旧 Session 兼容协议重新进入生产边界:\n{}",
        violations.join("\n")
    );

    for removed in [
        "crates/mai-runtime/src/agent_host/session_wire.rs",
        "crates/mai-runtime/src/agent_host/sessions.rs",
        "crates/mai-runtime/src/runtime_session_events.rs",
        "crates/mai-server/web/src/events/session-events.generated.ts",
        "crates/mai-server/web/src/events/session-store.ts",
    ] {
        assert!(
            !workspace.join(removed).exists(),
            "旧兼容文件仍存在: {removed}"
        );
    }
}

fn scan(path: &Path, violations: &mut Vec<String>) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).expect("read source directory") {
            scan(&entry.expect("source entry").path(), violations);
        }
        return;
    }
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return;
    };
    if !matches!(extension, "rs" | "ts" | "tsx") || is_rust_test_file(path) {
        return;
    }
    let source = std::fs::read_to_string(path).expect("read source file");
    for forbidden in FORBIDDEN {
        if source.contains(forbidden) {
            violations.push(format!("{}: `{forbidden}`", path.display()));
        }
    }
}

fn is_rust_test_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    path.extension().and_then(|extension| extension.to_str()) == Some("rs")
        && (name == "tests.rs" || name.ends_with("_tests.rs"))
}
