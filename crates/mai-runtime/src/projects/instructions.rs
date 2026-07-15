use std::path::{Path, PathBuf};

use pl_core::shell_quote_word;

use crate::projects::mcp::PROJECT_WORKSPACE_PATH;
use crate::{Result, RuntimeError};

const PROJECT_INSTRUCTION_CANDIDATE_FILES: [&str; 3] =
    ["AGENTS.override.md", "AGENTS.md", "Agents.md"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectInstructionSourceFile {
    pub(crate) file_name: String,
    pub(crate) container_path: String,
    pub(crate) host_path: Option<PathBuf>,
}

pub(crate) fn detect_existing_files_command() -> String {
    PROJECT_INSTRUCTION_CANDIDATE_FILES
        .iter()
        .map(|file_name| {
            let container_path = format!("{PROJECT_WORKSPACE_PATH}/{file_name}");
            format!(
                "if [ -f {path} ]; then printf '%s\\t%s\\n' {file_name} {path}; exit 0; fi",
                file_name = shell_quote_word(file_name),
                path = shell_quote_word(&container_path),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn detected_files_from_stdout(
    stdout: &str,
) -> Result<Vec<ProjectInstructionSourceFile>> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(detected_file_from_line)
        .collect()
}

pub(crate) fn load_workspace_instructions(stage_root: &Path) -> Result<String> {
    pl_core::load_workspace_instructions(stage_root).map_err(|err| {
        RuntimeError::InvalidInput(format!(
            "project workspace instruction discovery failed: {err}"
        ))
    })
}

fn detected_file_from_line(line: &str) -> Result<ProjectInstructionSourceFile> {
    let parts = line.split('\t').collect::<Vec<_>>();
    let [file_name, container_path] = parts.as_slice() else {
        return Err(RuntimeError::InvalidInput(format!(
            "invalid project instruction source listing: {line}"
        )));
    };
    if !PROJECT_INSTRUCTION_CANDIDATE_FILES
        .iter()
        .any(|candidate| candidate == file_name)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "unsupported project instruction source listing: {line}"
        )));
    }
    let expected_path = format!("{PROJECT_WORKSPACE_PATH}/{file_name}");
    if *container_path != expected_path {
        return Err(RuntimeError::InvalidInput(format!(
            "unexpected project instruction source path: {container_path}"
        )));
    }
    Ok(ProjectInstructionSourceFile {
        file_name: (*file_name).to_string(),
        container_path: (*container_path).to_string(),
        host_path: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_detected_project_instruction_file() {
        let files =
            detected_files_from_stdout("AGENTS.md\t/workspace/repo/AGENTS.md\n").expect("parse");

        assert_eq!(
            files,
            vec![ProjectInstructionSourceFile {
                file_name: "AGENTS.md".to_string(),
                container_path: "/workspace/repo/AGENTS.md".to_string(),
                host_path: None,
            }]
        );
    }

    #[test]
    fn rejects_unsupported_project_instruction_file() {
        let err = detected_files_from_stdout("README.md\t/workspace/repo/README.md\n")
            .expect_err("reject source");

        assert!(
            err.to_string()
                .contains("unsupported project instruction source")
        );
    }
}
