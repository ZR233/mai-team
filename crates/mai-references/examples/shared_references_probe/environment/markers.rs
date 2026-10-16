//! 三类数据标记（可写层 / 工作区 / 匿名卷）：新建环境时写入一次，之后只读校验。

use super::container::{exec, read_text_file, shell_quote};
use super::*;

/// 一条数据标记的观察结果。
#[derive(Debug, Clone, Serialize)]
pub struct MarkerFact {
    pub path: String,
    pub expected: String,
    pub observed: Option<String>,
    /// written（本轮写入并读回）/ present（已存在且内容一致）/ missing /
    /// content_mismatch / write_failed / unreadable
    pub state: String,
}

/// 三类数据标记：已存在就校验内容，不存在则在新建环境时写入，绝不悄悄修复。
pub async fn ensure_markers(
    probe: &Probe,
    agent: &AgentFixture,
    container_id: &str,
    created: bool,
) -> Result<Vec<MarkerFact>> {
    let wanted = [
        (MARKER_USR_LOCAL, marker_content(agent.key, "usr-local")),
        (MARKER_WORKSPACE, marker_content(agent.key, "workspace")),
        (MARKER_ANON, marker_content(agent.key, "anon")),
    ];
    let mut markers = Vec::new();
    for (path, expected) in wanted {
        let observed = read_text_file(probe, container_id, path).await?;
        if matches!(observed.as_deref(), Some(value) if value.trim() == expected.as_str()) {
            markers.push(MarkerFact {
                path: path.to_string(),
                expected,
                observed,
                state: "present".to_string(),
            });
            continue;
        }
        if let Some(value) = observed {
            markers.push(MarkerFact {
                path: path.to_string(),
                expected,
                observed: Some(value),
                state: "content_mismatch".to_string(),
            });
            continue;
        }
        if !created {
            markers.push(MarkerFact {
                path: path.to_string(),
                expected,
                observed: None,
                state: "missing".to_string(),
            });
            continue;
        }
        // 新建环境：写入标记并立刻读回；读不回来就是 write_failed，不当作成功。
        let directory = path
            .rsplit_once('/')
            .map(|(directory, _)| directory.to_string())
            .unwrap_or_else(|| "/".to_string());
        let command = format!(
            "mkdir -p {directory} && printf '%s' {content} > {path}",
            directory = shell_quote(&directory),
            content = shell_quote(&expected),
            path = shell_quote(path),
        );
        let output = exec(probe, container_id, &command).await?;
        if output.status != 0 {
            markers.push(MarkerFact {
                path: path.to_string(),
                expected,
                observed: None,
                state: "write_failed".to_string(),
            });
            continue;
        }
        match read_text_file(probe, container_id, path).await? {
            Some(value) if value.trim() == expected.as_str() => markers.push(MarkerFact {
                path: path.to_string(),
                expected,
                observed: Some(value),
                state: "written".to_string(),
            }),
            Some(value) => markers.push(MarkerFact {
                path: path.to_string(),
                expected,
                observed: Some(value),
                state: "content_mismatch".to_string(),
            }),
            None => markers.push(MarkerFact {
                path: path.to_string(),
                expected,
                observed: None,
                state: "write_failed".to_string(),
            }),
        }
    }
    Ok(markers)
}

/// 只读地读三类标记（不写）：用于判定保留性，不能被当成“修复”。
pub async fn read_markers(
    probe: &Probe,
    agent_key: &str,
    container_id: &str,
) -> Result<Vec<MarkerFact>> {
    let wanted = [
        (MARKER_USR_LOCAL, marker_content(agent_key, "usr-local")),
        (MARKER_WORKSPACE, marker_content(agent_key, "workspace")),
        (MARKER_ANON, marker_content(agent_key, "anon")),
    ];
    let mut markers = Vec::new();
    for (path, expected) in wanted {
        let observed = read_text_file(probe, container_id, path).await?;
        let state = match observed.as_deref() {
            Some(value) if value.trim() == expected.as_str() => "present",
            Some(_) => "content_mismatch",
            None => "missing",
        };
        markers.push(MarkerFact {
            path: path.to_string(),
            expected,
            observed,
            state: state.to_string(),
        });
    }
    Ok(markers)
}

/// 标记的确定内容：同一 Agent 在任何轮次都必须读到同样的值。
pub fn marker_content(agent_key: &str, kind: &str) -> String {
    format!("mai-acceptance:{agent_key}:{kind}:v1")
}
