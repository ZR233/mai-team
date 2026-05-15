use crate::error::{RelayErrorKind, RelayResult};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) fn set_executable_permissions(path: &Path) -> RelayResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _path = path;
    }
    Ok(())
}

pub(super) fn current_executable_path() -> RelayResult<PathBuf> {
    Ok(fs::canonicalize(std::env::current_exe()?)?)
}

pub(super) fn backup_path_for(executable_path: &Path) -> RelayResult<PathBuf> {
    let file_name = executable_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            RelayErrorKind::InvalidInput("relay executable path has no file name".to_string())
        })?;
    Ok(executable_path.with_file_name(format!("{file_name}.backup")))
}

pub(super) fn replace_binary(executable_path: &Path, new_binary_path: &Path) -> RelayResult<()> {
    let backup_path = backup_path_for(executable_path)?;
    replace_binary_with_backup_path(executable_path, new_binary_path, &backup_path)
}

fn replace_binary_with_backup_path(
    executable_path: &Path,
    new_binary_path: &Path,
    backup_path: &Path,
) -> RelayResult<()> {
    if backup_path.exists() {
        fs::remove_file(backup_path)?;
    }
    fs::rename(executable_path, backup_path)?;
    match fs::rename(new_binary_path, executable_path) {
        Ok(()) => Ok(()),
        Err(error) => match fs::rename(backup_path, executable_path) {
            Ok(()) => Err(RelayErrorKind::InvalidInput(format!(
                "failed to replace relay binary and restored backup: {error}"
            ))),
            Err(restore_error) => Err(RelayErrorKind::InvalidInput(format!(
                "failed to replace relay binary ({error}) and failed to restore backup ({restore_error})"
            ))),
        },
    }
}

pub(super) fn rollback_binary(executable_path: &Path, backup_path: &Path) -> RelayResult<()> {
    let rollback_path = rollback_path_for(executable_path)?;
    if rollback_path.exists() {
        fs::remove_file(&rollback_path)?;
    }
    fs::rename(executable_path, &rollback_path)?;
    match fs::rename(backup_path, executable_path) {
        Ok(()) => {
            let _ = fs::remove_file(&rollback_path);
            Ok(())
        }
        Err(error) => match fs::rename(&rollback_path, executable_path) {
            Ok(()) => Err(RelayErrorKind::InvalidInput(format!(
                "failed to restore relay backup and kept current binary: {error}"
            ))),
            Err(restore_error) => Err(RelayErrorKind::InvalidInput(format!(
                "failed to restore relay backup ({error}) and failed to keep current binary ({restore_error})"
            ))),
        },
    }
}

fn rollback_path_for(executable_path: &Path) -> RelayResult<PathBuf> {
    let file_name = executable_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            RelayErrorKind::InvalidInput("relay executable path has no file name".to_string())
        })?;
    Ok(executable_path.with_file_name(format!("{file_name}.rollback")))
}

pub(super) fn schedule_restart(delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn failed_replace_restores_backup() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let executable_path = temp_dir.path().join("mai-relay");
        let missing_new_binary_path = temp_dir.path().join("mai-relay.new");
        let backup_path = temp_dir.path().join("mai-relay.backup");
        fs::write(&executable_path, "old").expect("write current");

        let error = replace_binary_with_backup_path(
            &executable_path,
            &missing_new_binary_path,
            &backup_path,
        )
        .expect_err("replacement fails");

        assert!(error.to_string().contains("restored backup"));
        assert_eq!(
            fs::read_to_string(&executable_path).expect("read current"),
            "old"
        );
        assert!(!backup_path.exists());
    }
}
