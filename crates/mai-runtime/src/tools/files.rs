use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use mai_docker::DockerClient;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::{NamedTempFile, tempdir};
use tokio_util::sync::CancellationToken;

use crate::{Result, RuntimeError};

const DEFAULT_READ_FILE_BYTES: usize = 50 * 1024;
const MAX_READ_FILE_BYTES: usize = 512 * 1024;
const DEFAULT_LIST_FILES_LIMIT: usize = 200;
const MAX_LIST_FILES_LIMIT: usize = 1_000;
const DEFAULT_SEARCH_MATCH_LIMIT: usize = 100;
const MAX_SEARCH_MATCH_LIMIT: usize = 2_000;
const MAX_SEARCH_OUTPUT_TEXT_BYTES: usize = 4 * 1024;

pub(crate) struct ContainerFileTools<'a> {
    docker: &'a DockerClient,
    container_id: &'a str,
}

impl<'a> ContainerFileTools<'a> {
    pub(crate) fn new(docker: &'a DockerClient, container_id: &'a str) -> Self {
        Self {
            docker,
            container_id,
        }
    }

    pub(crate) async fn read_file(&self, arguments: &Value) -> Result<Value> {
        let path = required_string(arguments, "path")?;
        let cwd = optional_string(arguments, "cwd");
        let line_start = optional_usize(arguments, "line_start")?;
        let line_count = optional_usize(arguments, "line_count")?;
        let offset = optional_usize(arguments, "offset")?.unwrap_or(0);
        let max_bytes = optional_usize(arguments, "max_bytes")?
            .unwrap_or(DEFAULT_READ_FILE_BYTES)
            .clamp(1, MAX_READ_FILE_BYTES);
        if line_start.is_some() && offset > 0 {
            return Err(RuntimeError::InvalidInput(
                "read_file cannot combine line_start with offset".to_string(),
            ));
        }
        let command = if let Some(line_start) = line_start {
            let line_count = line_count.unwrap_or(200).clamp(1, 10_000);
            let end = line_start.saturating_add(line_count).saturating_sub(1);
            format!(
                "if [ ! -f {path} ]; then echo __MAI_FILE_MISSING__; exit 0; fi; sed -n '{start},{end}p' {path}",
                path = shell_quote(&path),
                start = line_start,
                end = end
            )
        } else {
            format!(
                "if [ ! -f {path} ]; then echo __MAI_FILE_MISSING__; exit 0; fi; dd if={path} bs=1 skip={offset} count={count} 2>/dev/null",
                path = shell_quote(&path),
                offset = offset,
                count = max_bytes.saturating_add(1)
            )
        };
        let output = self
            .docker
            .exec_shell(self.container_id, &command, cwd.as_deref(), Some(20))
            .await?;
        if output.stdout.trim() == "__MAI_FILE_MISSING__" {
            return Err(RuntimeError::InvalidInput(format!(
                "file not found: {path}"
            )));
        }
        if output.status != 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "read_file failed: {}",
                preview_error(&output.stderr, &output.stdout)
            )));
        }
        let (text, truncated, bytes_omitted, next_offset) =
            bounded_text(&output.stdout, max_bytes, offset);
        Ok(json!({
            "path": path,
            "offset": offset,
            "bytes_returned": text.len(),
            "bytes_omitted": bytes_omitted,
            "truncated": truncated,
            "next_offset": next_offset,
            "text": text,
        }))
    }

    pub(crate) async fn list_files(&self, arguments: &Value) -> Result<Value> {
        let path = optional_string(arguments, "path").unwrap_or_else(|| ".".to_string());
        let cwd = optional_string(arguments, "cwd");
        let glob = optional_string(arguments, "glob")
            .or_else(|| optional_string(arguments, "pattern"))
            .unwrap_or_else(|| "*".to_string());
        let max_files = optional_usize(arguments, "max_files")?
            .unwrap_or(DEFAULT_LIST_FILES_LIMIT)
            .clamp(1, MAX_LIST_FILES_LIMIT);
        let include_dirs = optional_bool(arguments, "include_dirs").unwrap_or(false);
        let limit = max_files.saturating_add(1);
        let rg_command = format!(
            "if command -v rg >/dev/null 2>&1; then rg --files -g {glob} {path} | sort | head -n {limit}; else exit 127; fi",
            path = shell_quote(&path),
            glob = shell_quote(&glob),
            limit = limit
        );
        let mut output = self
            .docker
            .exec_shell(self.container_id, &rg_command, cwd.as_deref(), Some(20))
            .await?;
        if output.status == 127 {
            let type_filter = if include_dirs { "" } else { "-type f " };
            let command = format!(
                "find {path} {type_filter}-name {glob} | sort | head -n {limit}",
                path = shell_quote(&path),
                type_filter = type_filter,
                glob = shell_quote(&glob),
                limit = limit
            );
            output = self
                .docker
                .exec_shell(self.container_id, &command, cwd.as_deref(), Some(20))
                .await?;
        } else if include_dirs {
            let dir_command = format!(
                "find {path} -type d -name {glob} | sort | head -n {limit}",
                path = shell_quote(&path),
                glob = shell_quote(&glob),
                limit = limit
            );
            let dirs = self
                .docker
                .exec_shell(self.container_id, &dir_command, cwd.as_deref(), Some(20))
                .await?;
            if dirs.status == 0 {
                output.stdout.push_str(&dirs.stdout);
            }
        }
        if output.status != 0 && output.status != 1 {
            return Err(RuntimeError::InvalidInput(format!(
                "list_files failed: {}",
                preview_error(&output.stderr, &output.stdout)
            )));
        }
        let mut entries = output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(max_files.saturating_add(1))
            .collect::<Vec<_>>();
        let truncated = entries.len() > max_files;
        entries.truncate(max_files);
        Ok(json!({
            "path": path,
            "glob": glob,
            "include_dirs": include_dirs,
            "files": entries,
            "count": entries.len(),
            "truncated": truncated,
        }))
    }

    pub(crate) async fn search_files(
        &self,
        arguments: &Value,
        cancellation_token: &CancellationToken,
    ) -> Result<Value> {
        let query = required_string(arguments, "query")?;
        let path = optional_string(arguments, "path").unwrap_or_else(|| ".".to_string());
        let cwd = optional_string(arguments, "cwd");
        let glob = optional_string(arguments, "glob");
        let case_sensitive = optional_bool(arguments, "case_sensitive").unwrap_or(true);
        let literal = optional_bool(arguments, "literal").unwrap_or(false);
        let context_lines = optional_usize(arguments, "context_lines")?
            .unwrap_or(0)
            .min(20);
        let max_matches = optional_usize(arguments, "max_matches")?
            .unwrap_or(DEFAULT_SEARCH_MATCH_LIMIT)
            .clamp(1, MAX_SEARCH_MATCH_LIMIT);
        let mut args = vec![
            "rg".to_string(),
            "--json".to_string(),
            "--line-number".to_string(),
            "--column".to_string(),
            "--max-count".to_string(),
            max_matches.to_string(),
        ];
        if !case_sensitive {
            args.push("--ignore-case".to_string());
        }
        if literal {
            args.push("--fixed-strings".to_string());
        }
        if context_lines > 0 {
            args.push("--context".to_string());
            args.push(context_lines.to_string());
        }
        if let Some(glob) = &glob {
            args.push("--glob".to_string());
            args.push(glob.clone());
        }
        args.push("--".to_string());
        args.push(query.clone());
        args.push(path.clone());
        let command = format!(
            "if command -v rg >/dev/null 2>&1; then {rg}; else exit 127; fi",
            rg = shell_command(&args)
        );
        let output = self
            .docker
            .exec_shell_with_cancel(
                self.container_id,
                &command,
                cwd.as_deref(),
                Some(30),
                cancellation_token,
            )
            .await?;
        if output.status == 1 {
            return Ok(json!({
                "query": query,
                "path": path,
                "glob": glob,
                "matches": [],
                "count": 0,
                "truncated": false,
            }));
        }
        if output.status == 127 {
            return self
                .search_files_with_grep(
                    query,
                    path,
                    cwd,
                    glob,
                    case_sensitive,
                    literal,
                    max_matches,
                    cancellation_token,
                )
                .await;
        }
        if output.status != 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "search_files failed: {}",
                preview_error(&output.stderr, &output.stdout)
            )));
        }
        let mut matches = Vec::new();
        for line in output.stdout.lines() {
            if matches.len() >= max_matches {
                break;
            }
            let Ok(event) = serde_json::from_str::<RgJsonEvent>(line) else {
                continue;
            };
            if event.kind != "match" {
                continue;
            }
            let Some(data) = event.data else {
                continue;
            };
            let text = data.lines.text;
            let (text, _, _, _) = bounded_text(&text, MAX_SEARCH_OUTPUT_TEXT_BYTES, 0);
            let column = data
                .submatches
                .first()
                .map(|m| m.start.saturating_add(1))
                .unwrap_or(1);
            matches.push(json!({
                "path": data.path.text,
                "line": data.line_number.unwrap_or(0),
                "column": column,
                "text": text,
            }));
        }
        Ok(json!({
            "query": query,
            "path": path,
            "glob": glob,
            "matches": matches,
            "count": matches.len(),
            "truncated": matches.len() >= max_matches,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn search_files_with_grep(
        &self,
        query: String,
        path: String,
        cwd: Option<String>,
        glob: Option<String>,
        case_sensitive: bool,
        literal: bool,
        max_matches: usize,
        cancellation_token: &CancellationToken,
    ) -> Result<Value> {
        let mut grep_args = vec![
            "grep".to_string(),
            "-R".to_string(),
            "-n".to_string(),
            "-H".to_string(),
        ];
        if !case_sensitive {
            grep_args.push("-i".to_string());
        }
        if literal {
            grep_args.push("-F".to_string());
        }
        grep_args.push("--".to_string());
        grep_args.push(query.clone());
        grep_args.push(path.clone());
        let grep = shell_command(&grep_args);
        let command = if let Some(glob) = &glob {
            format!(
                "{grep} | grep {} | head -n {}",
                shell_quote(glob),
                max_matches.saturating_add(1)
            )
        } else {
            format!("{grep} | head -n {}", max_matches.saturating_add(1))
        };
        let output = self
            .docker
            .exec_shell_with_cancel(
                self.container_id,
                &command,
                cwd.as_deref(),
                Some(30),
                cancellation_token,
            )
            .await?;
        if output.status != 0 && output.stdout.trim().is_empty() {
            return Ok(json!({
                "query": query,
                "path": path,
                "glob": glob,
                "matches": [],
                "count": 0,
                "truncated": false,
            }));
        }
        let mut matches = Vec::new();
        for raw in output.stdout.lines().take(max_matches.saturating_add(1)) {
            if matches.len() >= max_matches {
                break;
            }
            if let Some((file, rest)) = raw.split_once(':')
                && let Some((line, text)) = rest.split_once(':')
            {
                matches.push(json!({
                    "path": file,
                    "line": line.parse::<usize>().unwrap_or(0),
                    "column": 1,
                    "text": text,
                }));
            }
        }
        Ok(json!({
            "query": query,
            "path": path,
            "glob": glob,
            "matches": matches,
            "count": matches.len(),
            "truncated": output.stdout.lines().count() > max_matches,
        }))
    }

    pub(crate) async fn apply_patch(&self, arguments: &Value) -> Result<Value> {
        let input = required_string(arguments, "input")?;
        let cwd = optional_string(arguments, "cwd");
        let cwd = self.resolve_patch_cwd(cwd).await?;
        let parsed = Patch::parse(&input)?;
        let mut working = Vec::new();
        for operation in &parsed.operations {
            let source = resolve_relative_container_path(&cwd, operation.path())?;
            let destination = operation
                .move_to()
                .map(|path| resolve_relative_container_path(&cwd, path))
                .transpose()?;
            let existing = match operation {
                PatchOperation::Add { .. } => None,
                PatchOperation::Update { .. } | PatchOperation::Delete { .. } => {
                    Some(self.read_container_text(&source).await?)
                }
            };
            let change = operation.compute(existing.as_deref())?;
            working.push(PreparedPatchOperation {
                source,
                destination,
                change,
            });
        }

        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut deleted = Vec::new();
        let mut moved = Vec::new();
        for operation in working {
            match operation.change {
                PatchChange::Add { content } => {
                    self.write_container_text(&operation.source, &content)
                        .await?;
                    added.push(operation.source);
                }
                PatchChange::Update { content } => {
                    if let Some(destination) = operation.destination {
                        self.write_container_text(&destination, &content).await?;
                        if destination != operation.source {
                            self.remove_container_path(&operation.source).await?;
                            moved.push(json!({
                                "from": operation.source,
                                "to": destination,
                            }));
                        }
                    } else {
                        self.write_container_text(&operation.source, &content)
                            .await?;
                    }
                    updated.push(operation.source);
                }
                PatchChange::Delete => {
                    self.remove_container_path(&operation.source).await?;
                    deleted.push(operation.source);
                }
            }
        }
        let mut changed_files = BTreeSet::new();
        changed_files.extend(added.iter().cloned());
        changed_files.extend(updated.iter().cloned());
        changed_files.extend(deleted.iter().cloned());
        for item in &moved {
            if let Some(to) = item.get("to").and_then(Value::as_str) {
                changed_files.insert(to.to_string());
            }
        }
        Ok(json!({
            "cwd": cwd,
            "added": added,
            "updated": updated,
            "deleted": deleted,
            "moved": moved,
            "changed_files": changed_files.into_iter().collect::<Vec<_>>(),
            "stdout": "apply_patch completed",
            "stderr": "",
        }))
    }

    async fn resolve_patch_cwd(&self, cwd: Option<String>) -> Result<String> {
        if let Some(cwd) = cwd {
            return Ok(cwd);
        }
        let output = self
            .docker
            .exec_shell(
                self.container_id,
                "if [ -d /workspace/repo ]; then printf /workspace/repo; else printf /workspace; fi",
                Some("/"),
                Some(10),
            )
            .await?;
        if output.status != 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "apply_patch failed to resolve cwd: {}",
                preview_error(&output.stderr, &output.stdout)
            )));
        }
        Ok(output.stdout.trim().to_string())
    }

    async fn read_container_text(&self, container_path: &str) -> Result<String> {
        let command = format!("test -f -- {}", shell_quote(container_path));
        let output = self
            .docker
            .exec_shell(self.container_id, &command, Some("/"), Some(10))
            .await?;
        if output.status != 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "file not found: {container_path}"
            )));
        }

        let dir = tempdir()?;
        let host_path = dir.path().join("file");
        self.docker
            .copy_from_container_to_file(self.container_id, container_path, &host_path)
            .await
            .map_err(|err| {
                RuntimeError::InvalidInput(format!(
                    "failed to read `{container_path}` for apply_patch: {err}"
                ))
            })?;
        Ok(std::fs::read_to_string(&host_path)?)
    }

    async fn write_container_text(&self, container_path: &str, content: &str) -> Result<()> {
        let temp = NamedTempFile::new()?;
        std::fs::write(temp.path(), content)?;
        self.docker
            .copy_to_container(self.container_id, temp.path(), container_path)
            .await
            .map_err(|err| {
                RuntimeError::InvalidInput(format!(
                    "failed to write `{container_path}` for apply_patch: {err}"
                ))
            })
    }

    async fn remove_container_path(&self, container_path: &str) -> Result<()> {
        let command = format!("rm -f -- {}", shell_quote(container_path));
        let output = self
            .docker
            .exec_shell(self.container_id, &command, Some("/"), Some(20))
            .await?;
        if output.status != 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "failed to remove `{container_path}` for apply_patch: {}",
                preview_error(&output.stderr, &output.stdout)
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct RgJsonEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<RgJsonData>,
}

#[derive(Debug, Deserialize)]
struct RgJsonData {
    path: RgJsonText,
    lines: RgJsonText,
    line_number: Option<usize>,
    #[serde(default)]
    submatches: Vec<RgJsonSubmatch>,
}

#[derive(Debug, Deserialize)]
struct RgJsonText {
    text: String,
}

#[derive(Debug, Deserialize)]
struct RgJsonSubmatch {
    start: usize,
}

struct PreparedPatchOperation {
    source: String,
    destination: Option<String>,
    change: PatchChange,
}

enum PatchChange {
    Add { content: String },
    Update { content: String },
    Delete,
}

struct Patch {
    operations: Vec<PatchOperation>,
}

enum PatchOperation {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<PatchHunk>,
    },
    Delete {
        path: String,
    },
}

impl PatchOperation {
    fn path(&self) -> &str {
        match self {
            PatchOperation::Add { path, .. }
            | PatchOperation::Update { path, .. }
            | PatchOperation::Delete { path } => path,
        }
    }

    fn move_to(&self) -> Option<&str> {
        match self {
            PatchOperation::Update { move_to, .. } => move_to.as_deref(),
            _ => None,
        }
    }

    fn compute(&self, existing: Option<&str>) -> Result<PatchChange> {
        match self {
            PatchOperation::Add { lines, .. } => Ok(PatchChange::Add {
                content: lines.join(""),
            }),
            PatchOperation::Delete { .. } => {
                existing.ok_or_else(|| {
                    RuntimeError::InvalidInput("delete target does not exist".to_string())
                })?;
                Ok(PatchChange::Delete)
            }
            PatchOperation::Update { hunks, .. } => {
                let existing = existing.ok_or_else(|| {
                    RuntimeError::InvalidInput("update target does not exist".to_string())
                })?;
                let mut content_lines = split_preserving_newlines(existing);
                let mut search_start = 0;
                for hunk in hunks {
                    let old_lines = hunk
                        .lines
                        .iter()
                        .filter_map(|line| match line {
                            PatchLine::Context(text) | PatchLine::Remove(text) => {
                                Some(text.clone())
                            }
                            PatchLine::Add(_) => None,
                        })
                        .collect::<Vec<_>>();
                    let new_lines = hunk
                        .lines
                        .iter()
                        .filter_map(|line| match line {
                            PatchLine::Context(text) | PatchLine::Add(text) => Some(text.clone()),
                            PatchLine::Remove(_) => None,
                        })
                        .collect::<Vec<_>>();
                    if old_lines.is_empty() {
                        return Err(RuntimeError::InvalidInput(
                            "update hunk must include context or removed lines".to_string(),
                        ));
                    }
                    let index = find_subsequence(&content_lines, &old_lines, search_start)
                        .ok_or_else(|| {
                            RuntimeError::InvalidInput(
                                "apply_patch hunk context did not match target file".to_string(),
                            )
                        })?;
                    content_lines.splice(index..index + old_lines.len(), new_lines.clone());
                    search_start = index.saturating_add(new_lines.len());
                }
                Ok(PatchChange::Update {
                    content: content_lines.join(""),
                })
            }
        }
    }
}

struct PatchHunk {
    lines: Vec<PatchLine>,
}

enum PatchLine {
    Context(String),
    Add(String),
    Remove(String),
}

impl Patch {
    fn parse(input: &str) -> Result<Self> {
        let normalized = input.replace("\r\n", "\n");
        let mut lines = normalized.split_inclusive('\n').collect::<Vec<_>>();
        if normalized.ends_with('\n') {
            // split_inclusive already keeps all lines.
        } else if let Some(last) = normalized.rsplit('\n').next()
            && !last.is_empty()
            && lines.last().is_none_or(|line| *line != last)
        {
            lines.push(last);
        }
        let mut cursor = 0;
        expect_marker(&lines, &mut cursor, "*** Begin Patch")?;
        let mut operations = Vec::new();
        while cursor < lines.len() {
            let marker = trim_line_ending(lines[cursor]);
            if marker == "*** End Patch" {
                cursor += 1;
                break;
            }
            if let Some(path) = marker.strip_prefix("*** Add File: ") {
                cursor += 1;
                let mut content = Vec::new();
                while cursor < lines.len() {
                    let line = lines[cursor];
                    let trimmed = trim_line_ending(line);
                    if trimmed.starts_with("*** ") {
                        break;
                    }
                    let Some(rest) = line.strip_prefix('+') else {
                        return Err(RuntimeError::InvalidInput(
                            "add file lines must start with `+`".to_string(),
                        ));
                    };
                    content.push(rest.to_string());
                    cursor += 1;
                }
                operations.push(PatchOperation::Add {
                    path: path.to_string(),
                    lines: content,
                });
                continue;
            }
            if let Some(path) = marker.strip_prefix("*** Delete File: ") {
                cursor += 1;
                operations.push(PatchOperation::Delete {
                    path: path.to_string(),
                });
                continue;
            }
            if let Some(path) = marker.strip_prefix("*** Update File: ") {
                cursor += 1;
                let mut move_to = None;
                if cursor < lines.len()
                    && let Some(dest) =
                        trim_line_ending(lines[cursor]).strip_prefix("*** Move to: ")
                {
                    move_to = Some(dest.to_string());
                    cursor += 1;
                }
                let mut hunks = Vec::new();
                while cursor < lines.len() {
                    let marker = trim_line_ending(lines[cursor]);
                    if marker.starts_with("*** ") {
                        break;
                    }
                    if !marker.starts_with("@@") {
                        return Err(RuntimeError::InvalidInput(
                            "update hunks must start with `@@`".to_string(),
                        ));
                    }
                    cursor += 1;
                    let mut hunk_lines = Vec::new();
                    while cursor < lines.len() {
                        let line = lines[cursor];
                        let trimmed = trim_line_ending(line);
                        if trimmed.starts_with("@@") || trimmed.starts_with("*** ") {
                            break;
                        }
                        if trimmed == "*** End of File" {
                            cursor += 1;
                            break;
                        }
                        let Some(prefix) = line.chars().next() else {
                            return Err(RuntimeError::InvalidInput(
                                "empty hunk line is invalid".to_string(),
                            ));
                        };
                        let text = line[prefix.len_utf8()..].to_string();
                        match prefix {
                            ' ' => hunk_lines.push(PatchLine::Context(text)),
                            '+' => hunk_lines.push(PatchLine::Add(text)),
                            '-' => hunk_lines.push(PatchLine::Remove(text)),
                            _ => {
                                return Err(RuntimeError::InvalidInput(
                                    "hunk lines must start with space, `+`, or `-`".to_string(),
                                ));
                            }
                        }
                        cursor += 1;
                    }
                    hunks.push(PatchHunk { lines: hunk_lines });
                }
                if hunks.is_empty() {
                    return Err(RuntimeError::InvalidInput(
                        "update file requires at least one hunk".to_string(),
                    ));
                }
                operations.push(PatchOperation::Update {
                    path: path.to_string(),
                    move_to,
                    hunks,
                });
                continue;
            }
            return Err(RuntimeError::InvalidInput(format!(
                "invalid patch marker `{marker}`"
            )));
        }
        if cursor == 0 || operations.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "patch must include at least one operation".to_string(),
            ));
        }
        if cursor < lines.len() && lines[cursor..].iter().any(|line| !line.trim().is_empty()) {
            return Err(RuntimeError::InvalidInput(
                "unexpected content after patch end".to_string(),
            ));
        }
        Ok(Self { operations })
    }
}

fn expect_marker(lines: &[&str], cursor: &mut usize, expected: &str) -> Result<()> {
    if lines
        .get(*cursor)
        .map(|line| trim_line_ending(line) == expected)
        .unwrap_or(false)
    {
        *cursor += 1;
        return Ok(());
    }
    Err(RuntimeError::InvalidInput(format!(
        "patch must start with `{expected}`"
    )))
}

fn split_preserving_newlines(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut lines = value
        .split_inclusive('\n')
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !value.ends_with('\n')
        && let Some(last) = value.rsplit('\n').next()
    {
        if let Some(existing_last) = lines.last_mut() {
            if existing_last.ends_with('\n') {
                lines.push(last.to_string());
            }
        } else {
            lines.push(last.to_string());
        }
    }
    lines
}

fn find_subsequence(haystack: &[String], needle: &[String], start: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (start..=haystack.len().saturating_sub(needle.len()))
        .find(|&index| haystack[index..index + needle.len()] == *needle)
}

fn resolve_relative_container_path(cwd: &str, raw_path: &str) -> Result<String> {
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "patch file path cannot be empty".to_string(),
        ));
    }
    let path = Path::new(raw_path);
    if path.is_absolute() {
        return Err(RuntimeError::InvalidInput(
            "patch file paths must be relative".to_string(),
        ));
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(RuntimeError::InvalidInput(
                    "patch file paths cannot contain `..`".to_string(),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(RuntimeError::InvalidInput(
                    "patch file paths must be relative".to_string(),
                ));
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "patch file path cannot be empty".to_string(),
        ));
    }
    let mut full = PathBuf::from(cwd);
    full.push(clean);
    Ok(full.to_string_lossy().replace('\\', "/"))
}

fn trim_line_ending(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

fn required_string(arguments: &Value, field: &str) -> Result<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing string field `{field}`")))
}

fn optional_string(arguments: &Value, field: &str) -> Option<String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn optional_bool(arguments: &Value, field: &str) -> Option<bool> {
    arguments.get(field).and_then(Value::as_bool)
}

fn optional_usize(arguments: &Value, field: &str) -> Result<Option<usize>> {
    let Some(value) = arguments.get(field) else {
        return Ok(None);
    };
    let raw = value
        .as_u64()
        .ok_or_else(|| RuntimeError::InvalidInput(format!("field `{field}` must be an integer")))?;
    usize::try_from(raw)
        .map(Some)
        .map_err(|_| RuntimeError::InvalidInput(format!("field `{field}` is too large")))
}

fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

fn preview_error(stderr: &str, stdout: &str) -> String {
    preview(format!("{stderr}\n{stdout}").trim(), 500)
}

fn preview(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

fn bounded_text(
    value: &str,
    max_bytes: usize,
    offset: usize,
) -> (String, bool, usize, Option<usize>) {
    if value.len() <= max_bytes {
        return (value.to_string(), false, 0, None);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let text = value[..end].to_string();
    let omitted = value.len().saturating_sub(end);
    (text, true, omitted, Some(offset.saturating_add(end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_add_update_delete_and_move_parse() {
        let patch = Patch::parse(
            "*** Begin Patch\n*** Add File: a.txt\n+one\n*** Update File: b.txt\n*** Move to: c.txt\n@@\n-old\n+new\n*** Delete File: d.txt\n*** End Patch\n",
        )
        .expect("parse patch");
        assert_eq!(patch.operations.len(), 3);
    }

    #[test]
    fn patch_rejects_absolute_or_parent_paths() {
        assert!(resolve_relative_container_path("/workspace/repo", "/tmp/a").is_err());
        assert!(resolve_relative_container_path("/workspace/repo", "../a").is_err());
    }

    #[test]
    fn update_hunk_applies_context() {
        let operation = PatchOperation::Update {
            path: "a.txt".to_string(),
            move_to: None,
            hunks: vec![PatchHunk {
                lines: vec![
                    PatchLine::Context("one\n".to_string()),
                    PatchLine::Remove("two\n".to_string()),
                    PatchLine::Add("dos\n".to_string()),
                    PatchLine::Context("three\n".to_string()),
                ],
            }],
        };
        let PatchChange::Update { content } = operation.compute(Some("one\ntwo\nthree\n")).unwrap()
        else {
            panic!("expected update");
        };
        assert_eq!(content, "one\ndos\nthree\n");
    }
}
