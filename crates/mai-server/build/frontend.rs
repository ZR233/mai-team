use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const BUILD_FILES: &[&str] = &[
    "index.html",
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "tsconfig.json",
    "tsconfig.app.json",
    "tsconfig.node.json",
];

pub(crate) fn build(source_dir: &Path, build_dir: &Path, static_dir: &Path, npm_cache_dir: &Path) {
    watch(source_dir);
    prepare_build_dir(source_dir, build_dir);
    ensure_dependencies(build_dir, npm_cache_dir);

    let static_arg = static_dir.to_string_lossy().to_string();
    run_npm(
        build_dir,
        npm_cache_dir,
        [
            "run",
            "build",
            "--",
            "--outDir",
            static_arg.as_str(),
            "--emptyOutDir",
        ],
    );
    let index_path = static_dir.join("index.html");
    if !index_path.exists() {
        panic!(
            "frontend build did not produce {}; expected npm run build to create embedded static output",
            index_path.display()
        );
    }
}

fn watch(source_dir: &Path) {
    for name in BUILD_FILES {
        println!("cargo:rerun-if-changed={}", source_dir.join(name).display());
    }
    watch_dir(&source_dir.join("src"));
    watch_dir(&source_dir.join("public"));
}

fn prepare_build_dir(source_dir: &Path, build_dir: &Path) {
    fs::create_dir_all(build_dir).unwrap_or_else(|err| {
        panic!(
            "failed to create frontend build dir {}: {err}",
            build_dir.display()
        )
    });
    remove_stale_inputs(build_dir);
    copy_build_inputs(source_dir, build_dir);
}

fn remove_stale_inputs(build_dir: &Path) {
    for entry in fs::read_dir(build_dir).unwrap_or_else(|err| {
        panic!(
            "failed to read frontend build dir {}: {err}",
            build_dir.display()
        )
    }) {
        let entry =
            entry.unwrap_or_else(|err| panic!("failed to read frontend build entry: {err}"));
        if matches!(entry.file_name().to_str(), Some("node_modules")) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path).unwrap_or_else(|err| {
                panic!(
                    "failed to remove frontend build dir {}: {err}",
                    path.display()
                )
            });
        } else {
            fs::remove_file(&path).unwrap_or_else(|err| {
                panic!(
                    "failed to remove frontend build file {}: {err}",
                    path.display()
                )
            });
        }
    }
}

fn copy_build_inputs(source_dir: &Path, build_dir: &Path) {
    for name in BUILD_FILES {
        let source = source_dir.join(name);
        let target = build_dir.join(name);
        fs::copy(&source, &target).unwrap_or_else(|err| {
            panic!(
                "failed to copy frontend build input {} to {}: {err}",
                source.display(),
                target.display()
            )
        });
    }
    for name in ["src", "public"] {
        let source = source_dir.join(name);
        if source.is_dir() {
            copy_dir(&source, &build_dir.join(name));
        }
    }
}

fn copy_dir(source_dir: &Path, target_dir: &Path) {
    fs::create_dir_all(target_dir).unwrap_or_else(|err| {
        panic!(
            "failed to create frontend directory {}: {err}",
            target_dir.display()
        )
    });
    for entry in fs::read_dir(source_dir).unwrap_or_else(|err| {
        panic!(
            "failed to read frontend directory {}: {err}",
            source_dir.display()
        )
    }) {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read frontend entry: {err}"));
        if matches!(entry.file_name().to_str(), Some(".DS_Store")) {
            continue;
        }
        let source = entry.path();
        let target = target_dir.join(entry.file_name());
        if source.is_dir() {
            copy_dir(&source, &target);
        } else {
            fs::copy(&source, &target).unwrap_or_else(|err| {
                panic!(
                    "failed to copy frontend input {} to {}: {err}",
                    source.display(),
                    target.display()
                )
            });
        }
    }
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

fn ensure_dependencies(build_dir: &Path, npm_cache_dir: &Path) {
    run_npm(build_dir, npm_cache_dir, ["--version"]);
    if dependencies_missing(build_dir, npm_cache_dir) {
        install_dependencies(build_dir, npm_cache_dir);
    }
}

fn dependencies_missing(build_dir: &Path, npm_cache_dir: &Path) -> bool {
    !build_dir.join("node_modules").is_dir()
        || !local_npm_bin_exists(build_dir, "vite")
        || !npm_command_succeeds(build_dir, npm_cache_dir, ["ls", "--depth=0", "--silent"])
}

fn local_npm_bin_exists(build_dir: &Path, name: &str) -> bool {
    let bin_dir = build_dir.join("node_modules").join(".bin");
    if cfg!(windows) {
        bin_dir.join(format!("{name}.cmd")).exists()
    } else {
        bin_dir.join(name).exists()
    }
}

fn install_dependencies(build_dir: &Path, npm_cache_dir: &Path) {
    // 平台二进制由锁文件中的可选依赖提供；跳过第三方生命周期脚本可避免
    // Cargo 隔离构建环境在安装阶段执行下载内容，实际可执行性由后续 Vite 构建验证。
    if build_dir.join("package-lock.json").exists() {
        run_npm(
            build_dir,
            npm_cache_dir,
            ["ci", "--ignore-scripts", "--no-audit", "--no-fund"],
        );
    } else {
        run_npm(
            build_dir,
            npm_cache_dir,
            ["install", "--ignore-scripts", "--no-audit", "--no-fund"],
        );
    }
}

fn npm_command_succeeds<const N: usize>(
    build_dir: &Path,
    npm_cache_dir: &Path,
    args: [&str; N],
) -> bool {
    npm_command(build_dir, npm_cache_dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|err| {
            panic!(
                "failed to execute npm in {}; install Node.js/npm first: {err}",
                build_dir.display()
            )
        })
        .success()
}

fn run_npm<const N: usize>(build_dir: &Path, npm_cache_dir: &Path, args: [&str; N]) {
    let status = npm_command(build_dir, npm_cache_dir)
        .args(args)
        .status()
        .unwrap_or_else(|err| {
            panic!(
                "failed to execute npm in {}; install Node.js/npm first: {err}",
                build_dir.display()
            )
        });
    if !status.success() {
        panic!(
            "npm command failed in {} with status {status}",
            build_dir.display()
        );
    }
}

fn npm_command(build_dir: &Path, npm_cache_dir: &Path) -> Command {
    let mut command = Command::new("npm");
    command
        .current_dir(build_dir)
        .env("npm_config_cache", npm_cache_dir)
        .env("npm_config_audit", "false")
        .env("npm_config_fund", "false");
    command
}
