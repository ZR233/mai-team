//! 固定隔离目录内的真实文件系统验收；包含同文件系统 bind 子挂载与源根替换。
use chrono::Utc;
use mai_protocol::{NewReferenceSourceRequest, ReferenceErrorCode};
use mai_references::{
    ReferenceDeploymentPolicy, ready_source_record, revalidate_source, validate_new_source,
};
use serde_json::{Value, json};
use std::os::unix::{
    fs::{PermissionsExt, symlink},
    net::UnixListener,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

const ROOT: &str = "/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources";

fn request(path: &Path) -> NewReferenceSourceRequest {
    NewReferenceSourceRequest {
        name: "文件系统验收夹具".into(),
        host_path: path.to_string_lossy().into_owned(),
        revision_label: "fixed".into(),
        approved_for_agent_access: true,
    }
}

fn check(
    policy: &ReferenceDeploymentPolicy,
    path: &Path,
    name: &str,
    expected: &[ReferenceErrorCode],
    results: &mut Vec<Value>,
) {
    let result = validate_new_source(policy, &request(path), Utc::now());
    let (passed, code, detail) = match result {
        Ok(validated) => (
            expected.is_empty(),
            "ready".to_string(),
            json!({"files":validated.file_count(),"symlinks":validated.symlink_count()}),
        ),
        Err(error) => (
            expected.contains(&error.code()),
            error.code().to_string(),
            json!({"message":error.message()}),
        ),
    };
    results.push(json!({"case":name,"passed":passed,"code":code,"detail":detail}));
}

fn make_case(root: &Path, name: &str) -> std::io::Result<PathBuf> {
    let path = root.join(name);
    std::fs::create_dir(&path)?;
    std::fs::write(path.join("README.md"), "只读源码夹具\n")?;
    Ok(path)
}

fn run() -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let parent = Path::new(ROOT).join("sources");
    let policy = ReferenceDeploymentPolicy::new(vec![parent.clone()])?;
    let temporary = tempfile::Builder::new()
        .prefix("filesystem-")
        .tempdir_in(&parent)?;
    let root = temporary.path();
    let mut results = Vec::new();
    let valid = make_case(root, "valid")?;
    symlink("README.md", valid.join("internal-link"))?;
    check(&policy, &valid, "内部符号链接可以验证", &[], &mut results);
    let top_link = root.join("top-link");
    symlink(&valid, &top_link)?;
    check(
        &policy,
        &top_link,
        "源根符号链接拒绝",
        &[ReferenceErrorCode::UnsafeSymlink],
        &mut results,
    );
    let outer = make_case(root, "outside-link")?;
    let ancestor = make_case(root, "ancestor-real")?;
    let ancestor_repo = make_case(&ancestor, "repo")?;
    let ancestor_link = root.join("ancestor-link");
    symlink(&ancestor, &ancestor_link)?;
    check(
        &policy,
        &ancestor_link.join("repo"),
        "祖先符号链接拒绝",
        &[
            ReferenceErrorCode::UnsafeSymlink,
            ReferenceErrorCode::UnsafePathComponent,
        ],
        &mut results,
    );
    assert!(ancestor_repo.is_dir());
    symlink("/etc/passwd", outer.join("outside"))?;
    check(
        &policy,
        &outer,
        "内部外链拒绝",
        &[ReferenceErrorCode::UnsafeSymlink],
        &mut results,
    );
    let broken = make_case(root, "broken-link")?;
    symlink("missing", broken.join("broken"))?;
    check(
        &policy,
        &broken,
        "断链拒绝",
        &[ReferenceErrorCode::UnsafeSymlink],
        &mut results,
    );
    let cycle = make_case(root, "cycle")?;
    symlink("b", cycle.join("a"))?;
    symlink("a", cycle.join("b"))?;
    check(
        &policy,
        &cycle,
        "循环链接拒绝",
        &[ReferenceErrorCode::UnsafeSymlink],
        &mut results,
    );
    let hard = make_case(root, "hardlink")?;
    std::fs::hard_link(hard.join("README.md"), hard.join("copy"))?;
    check(
        &policy,
        &hard,
        "硬链接拒绝",
        &[ReferenceErrorCode::UnsafeHardlink],
        &mut results,
    );
    let fifo = make_case(root, "fifo")?;
    if !Command::new("mkfifo")
        .arg(fifo.join("pipe"))
        .status()?
        .success()
    {
        return Err("创建隔离 FIFO 失败".into());
    }
    check(
        &policy,
        &fifo,
        "FIFO 拒绝",
        &[ReferenceErrorCode::UnsafeFileType],
        &mut results,
    );
    let socket_case = make_case(root, "socket")?;
    let original_directory = std::env::current_dir()?;
    std::env::set_current_dir(&socket_case)?;
    let listener = UnixListener::bind("s");
    std::env::set_current_dir(original_directory)?;
    let _listener = listener?;

    check(
        &policy,
        &socket_case,
        "Unix socket 拒绝",
        &[ReferenceErrorCode::UnsafeFileType],
        &mut results,
    );
    let device = make_case(root, "device")?;
    if !Command::new("mknod")
        .arg(device.join("device"))
        .args(["c", "1", "3"])
        .status()?
        .success()
    {
        return Err("创建隔离设备节点失败".into());
    }
    check(
        &policy,
        &device,
        "设备节点拒绝",
        &[ReferenceErrorCode::UnsafeFileType],
        &mut results,
    );
    let git = make_case(root, "git")?;
    std::fs::create_dir(git.join(".git"))?;
    check(
        &policy,
        &git,
        "残留 Git 管理目录拒绝",
        &[ReferenceErrorCode::UnsafePathComponent],
        &mut results,
    );
    let writable = make_case(root, "writable")?;
    std::fs::set_permissions(
        writable.join("README.md"),
        std::fs::Permissions::from_mode(0o666),
    )?;
    check(
        &policy,
        &writable,
        "不可信可写文件拒绝",
        &[ReferenceErrorCode::UntrustedSourceOwner],
        &mut results,
    );
    let missing = root.join("missing");
    check(
        &policy,
        &missing,
        "缺失目录拒绝",
        &[ReferenceErrorCode::SourceNotFound],
        &mut results,
    );
    let submount = make_case(root, "submount")?;
    let mountpoint = submount.join("sub");
    std::fs::create_dir(&mountpoint)?;
    if !Command::new("mount")
        .arg("--bind")
        .arg(&valid)
        .arg(&mountpoint)
        .status()?
        .success()
    {
        return Err("创建隔离 bind 子挂载失败".into());
    }
    check(
        &policy,
        &submount,
        "同文件系统 bind 子挂载拒绝",
        &[ReferenceErrorCode::UnsafeInternalMount],
        &mut results,
    );
    if !Command::new("umount").arg(&mountpoint).status()?.success() {
        return Err("卸载隔离 bind 子挂载失败".into());
    }
    let replaced = make_case(root, "replaced")?;
    let validated = validate_new_source(&policy, &request(&replaced), Utc::now())?;
    let record = ready_source_record(
        Uuid::new_v4(),
        &request(&replaced),
        &validated,
        Utc::now(),
        Utc::now(),
    );
    std::fs::rename(&replaced, root.join("old-root"))?;
    make_case(root, "replaced")?;
    let failure =
        revalidate_source(&policy, &record, Utc::now()).expect_err("内容相同的新根不能沿用旧身份");
    results.push(json!({"case":"源根替换拒绝","passed":failure.code()==ReferenceErrorCode::SourceIdentityChanged,"code":failure.code().to_string()}));
    Ok(results)
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(results) => {
            let passed = results.iter().all(|row| row["passed"] == true);
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"passed":passed,"checks":results})).unwrap()
            );
            if passed {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("隔离文件系统验收失败：{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
