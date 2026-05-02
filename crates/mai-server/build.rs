use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let web_dir = manifest_dir.join("web");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let static_dir = out_dir.join("static");

    watch_frontend(&web_dir);
    ensure_npm(&web_dir);
    run_npm(&web_dir, ["run", "build"]);

    let dist_dir = web_dir.join("dist");
    if !dist_dir.join("index.html").exists() {
        panic!(
            "frontend build did not produce {}; expected npm run build to create Vite dist",
            dist_dir.join("index.html").display()
        );
    }

    if static_dir.exists() {
        fs::remove_dir_all(&static_dir).unwrap_or_else(|err| {
            panic!(
                "failed to remove old embedded static dir {}: {err}",
                static_dir.display()
            )
        });
    }
    copy_dir(&dist_dir, &static_dir);
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

fn ensure_npm(web_dir: &Path) {
    run_npm(web_dir, ["--version"]);
    if frontend_dependencies_missing(web_dir) {
        println!(
            "cargo:warning=frontend npm dependencies are missing or incomplete; installing them"
        );
        install_frontend_dependencies(web_dir);
    }
}

fn frontend_dependencies_missing(web_dir: &Path) -> bool {
    !web_dir.join("node_modules").is_dir()
        || !local_npm_bin_exists(web_dir, "vite")
        || !npm_command_succeeds(web_dir, ["ls", "--depth=0", "--silent"])
}

fn local_npm_bin_exists(web_dir: &Path, name: &str) -> bool {
    let bin_dir = web_dir.join("node_modules").join(".bin");
    if cfg!(windows) {
        bin_dir.join(format!("{name}.cmd")).exists()
    } else {
        bin_dir.join(name).exists()
    }
}

fn install_frontend_dependencies(web_dir: &Path) {
    if web_dir.join("package-lock.json").exists() {
        run_npm(web_dir, ["ci"]);
    } else {
        run_npm(web_dir, ["install"]);
    }
}

fn npm_command_succeeds<const N: usize>(web_dir: &Path, args: [&str; N]) -> bool {
    Command::new("npm")
        .args(args)
        .current_dir(web_dir)
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

fn run_npm<const N: usize>(web_dir: &Path, args: [&str; N]) {
    let status = Command::new("npm")
        .args(args)
        .current_dir(web_dir)
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

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to)
        .unwrap_or_else(|err| panic!("failed to create directory {}: {err}", to.display()));
    for entry in fs::read_dir(from)
        .unwrap_or_else(|err| panic!("failed to read directory {}: {err}", from.display()))
    {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read directory entry: {err}"));
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_dir(&source, &target);
        } else {
            if source.file_name() == Some(OsStr::new(".DS_Store")) {
                continue;
            }
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
