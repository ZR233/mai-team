//! 主机与 Docker daemon 的能力探测。
//!
//! 严格只读参考挂载依赖 Linux + 内核递归只读 + Docker 的递归挂载选项。任何一项不
//! 满足都必须 fail closed：不降级为只读 bind、不改用可写挂载继续创建容器。
//!
//! 连接目标同样 fail closed：不仅拒绝远程 `DOCKER_HOST`，还会解析实际生效的
//! docker context endpoint，ssh/tcp 远程 daemon 一律不接受，只允许本机 Unix socket。

use serde::Deserialize;
use tokio::process::Command;

use crate::client::{DockerClient, stderr_or_stdout};
use crate::error::{DockerError, Result};

/// 递归挂载选项所需的最低 Docker Engine 版本。
pub const MINIMUM_DOCKER_VERSION: (u64, u64) = (25, 0);
/// 严格递归只读挂载所需的最低 Linux 内核版本。
pub const MINIMUM_KERNEL_VERSION: (u64, u64) = (5, 12);
/// 没有显式 endpoint 时仍可沿用平台默认本机 socket 的 context 名。
pub const DEFAULT_DOCKER_CONTEXT: &str = "default";

/// 探测到的运行环境能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerCapabilities {
    pub os: String,
    pub server_version: String,
    pub kernel_version: String,
    pub api_version: Option<String>,
    pub recursive_read_only: bool,
}

impl DockerCapabilities {
    pub fn supports_strict_recursive_read_only(&self) -> bool {
        self.recursive_read_only
    }
}

impl DockerClient {
    /// 探测本机 Linux Docker daemon 是否满足严格只读挂载要求。
    ///
    /// 远程 `DOCKER_HOST`、被 `DOCKER_CONTEXT`／当前 context 指向的远程 daemon、
    /// 非 Linux 主机、缺少 daemon、旧版本 Engine/内核都返回错误。
    pub async fn probe_capabilities(&self) -> Result<DockerCapabilities> {
        if !cfg!(target_os = "linux") {
            return Err(DockerError::UnsupportedCapability(
                "严格只读参考挂载只支持 Linux 主机".to_string(),
            ));
        }
        match configured_docker_host() {
            // DOCKER_HOST 显式设置时它就是 CLI 真正连接的目标，context endpoint 不再生效。
            Some(host) => {
                if !is_supported_local_docker_endpoint(&host) {
                    return Err(DockerError::UnsupportedCapability(format!(
                        "远程 DOCKER_HOST `{host}` 不受支持：参考源码需要本机路径语义"
                    )));
                }
            }
            // 否则必须核验实际生效的 context endpoint，而不是只看环境变量。
            None => {
                let context = self.inspect_docker_context().await?;
                context.verify()?;
            }
        }

        let output = Command::new(&self.binary)
            .args(["version", "--format", "{{json .Server}}"])
            .output()
            .await
            .map_err(|error| DockerError::NotAvailable(error.to_string()))?;
        if !output.status.success() {
            return Err(DockerError::NotAvailable(stderr_or_stdout(&output)));
        }

        let server: DockerServerVersion = serde_json::from_str(&String::from_utf8(output.stdout)?)?;
        let os = server.os.unwrap_or_default();
        let server_version = server.version.unwrap_or_default();
        let kernel_version = server.kernel_version.unwrap_or_default();

        if os != "linux" {
            return Err(DockerError::UnsupportedCapability(format!(
                "Docker daemon 运行在 `{os}`，严格只读参考挂载只支持 linux"
            )));
        }
        if !meets_minimum_version(&server_version, MINIMUM_DOCKER_VERSION) {
            return Err(DockerError::UnsupportedCapability(format!(
                "Docker Engine {server_version} 低于递归挂载选项要求的最低版本 {}.{}",
                MINIMUM_DOCKER_VERSION.0, MINIMUM_DOCKER_VERSION.1
            )));
        }
        if !meets_minimum_version(&kernel_version, MINIMUM_KERNEL_VERSION) {
            return Err(DockerError::UnsupportedCapability(format!(
                "内核 {kernel_version} 低于严格递归只读要求的最低版本 {}.{}",
                MINIMUM_KERNEL_VERSION.0, MINIMUM_KERNEL_VERSION.1
            )));
        }

        Ok(DockerCapabilities {
            os,
            server_version,
            kernel_version,
            api_version: server.api_version,
            recursive_read_only: true,
        })
    }

    /// 解析实际生效的 docker context 及其 docker endpoint。
    ///
    /// `DOCKER_CONTEXT` 非空时直接核验该 context，否则先问 CLI 当前 context 名，
    /// 两者都不再依赖"只有环境变量才算远程"的假设。
    pub async fn inspect_docker_context(&self) -> Result<DockerContextEndpoint> {
        let explicit = configured_docker_context();
        let current = if explicit.is_none() {
            Some(self.docker_context_show().await?)
        } else {
            None
        };
        let name = effective_docker_context_name(explicit.as_deref(), current.as_deref())
            .ok_or_else(|| {
                DockerError::UnsupportedCapability(
                    "无法解析当前 docker context：请显式设置 DOCKER_CONTEXT".to_string(),
                )
            })?;

        let output = Command::new(&self.binary)
            .args([
                "context",
                "inspect",
                name.as_str(),
                "--format",
                "{{json .}}",
            ])
            .output()
            .await
            .map_err(|error| DockerError::NotAvailable(error.to_string()))?;
        if !output.status.success() {
            return Err(DockerError::NotAvailable(stderr_or_stdout(&output)));
        }
        parse_context_endpoint(&String::from_utf8(output.stdout)?)
    }

    async fn docker_context_show(&self) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(["context", "show"])
            .output()
            .await
            .map_err(|error| DockerError::NotAvailable(error.to_string()))?;
        if !output.status.success() {
            return Err(DockerError::NotAvailable(stderr_or_stdout(&output)));
        }
        let name = String::from_utf8(output.stdout)?.trim().to_string();
        if name.is_empty() {
            return Err(DockerError::UnsupportedCapability(
                "docker context show 没有返回当前 context 名".to_string(),
            ));
        }
        Ok(name)
    }
}

/// 一个 docker context 实际使用的连接目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerContextEndpoint {
    pub name: String,
    /// `Endpoints.docker.Host`；`None`／空串表示该 context 没有声明 endpoint。
    pub endpoint: Option<String>,
}

impl DockerContextEndpoint {
    /// 核验连接目标：只接受本机 Unix socket。
    pub fn verify(&self) -> Result<()> {
        verify_context_endpoint(self)
    }
}

/// 核验 context 的 endpoint。
///
/// 没有声明 endpoint 时，只有 `default` context 可以沿用 Docker CLI 在 Linux 上的
/// 平台默认本机 socket；其它 context 一律 fail closed，不能假定它是本机。
pub fn verify_context_endpoint(context: &DockerContextEndpoint) -> Result<()> {
    let endpoint = context
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match endpoint {
        Some(endpoint) => {
            if is_supported_local_docker_endpoint(endpoint) {
                Ok(())
            } else {
                Err(DockerError::UnsupportedCapability(format!(
                    "docker context `{}` 的 endpoint `{endpoint}` 不是本机 Unix socket：参考源码需要本机路径语义",
                    context.name
                )))
            }
        }
        None if context.name == DEFAULT_DOCKER_CONTEXT => Ok(()),
        None => Err(DockerError::UnsupportedCapability(format!(
            "docker context `{}` 没有可核验的 endpoint：请显式配置本机 Unix socket",
            context.name
        ))),
    }
}

/// 解析 `docker context inspect <name> --format '{{json .}}'` 的输出。
pub fn parse_context_endpoint(json: &str) -> Result<DockerContextEndpoint> {
    let inspected: InspectContext = serde_json::from_str(json)?;
    let name = inspected
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_DOCKER_CONTEXT.to_string());
    let endpoint = inspected
        .endpoints
        .and_then(|endpoints| endpoints.docker)
        .and_then(|docker| docker.host)
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty());
    Ok(DockerContextEndpoint { name, endpoint })
}

/// 需要核验的 context 名：`DOCKER_CONTEXT` 非空时优先，否则用 CLI 报告的当前 context。
pub fn effective_docker_context_name(
    explicit: Option<&str>,
    current: Option<&str>,
) -> Option<String> {
    let requested = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| current.map(str::trim).filter(|value| !value.is_empty()))?;
    Some(requested.to_string())
}

/// 允许的 docker endpoint：`unix://<绝对路径>` 或裸绝对路径。
pub fn is_supported_local_docker_endpoint(endpoint: &str) -> bool {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return false;
    }
    match endpoint.strip_prefix("unix://") {
        Some(path) => path.starts_with('/'),
        None => endpoint.starts_with('/'),
    }
}

/// `DOCKER_HOST` 的实际取值；空值等同于未设置（与 Docker CLI 语义一致）。
fn configured_docker_host() -> Option<String> {
    std::env::var_os("DOCKER_HOST")
        .and_then(|value| value.into_string().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `DOCKER_CONTEXT` 的实际取值；空值等同于未设置。
fn configured_docker_context() -> Option<String> {
    std::env::var_os("DOCKER_CONTEXT")
        .and_then(|value| value.into_string().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Deserialize)]
struct InspectContext {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Endpoints")]
    endpoints: Option<InspectContextEndpoints>,
}

#[derive(Debug, Deserialize)]
struct InspectContextEndpoints {
    docker: Option<InspectContextEndpoint>,
}

#[derive(Debug, Deserialize)]
struct InspectContextEndpoint {
    #[serde(rename = "Host")]
    host: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerServerVersion {
    #[serde(rename = "Os")]
    os: Option<String>,
    #[serde(rename = "Version")]
    version: Option<String>,
    #[serde(rename = "KernelVersion")]
    kernel_version: Option<String>,
    #[serde(rename = "ApiVersion")]
    api_version: Option<String>,
}

/// 解析版本号的前三段数字；无法解析时返回 `None`。
pub fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value
        .trim()
        .trim_start_matches('v')
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty());
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    let patch = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    Some((major, minor, patch))
}

/// 版本是否达到给定的 `(major, minor)` 下限；无法解析时按不满足处理。
pub fn meets_minimum_version(value: &str, minimum: (u64, u64)) -> bool {
    parse_version(value).is_some_and(|(major, minor, _)| (major, minor) >= minimum)
}

/// 非本机 unix socket 的 `DOCKER_HOST` 不受支持；空值等同于未设置。
pub fn is_unsupported_docker_host(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !is_supported_local_docker_endpoint(value)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_version_prefixes_with_suffixes() {
        assert_eq!(parse_version("25.0.1"), Some((25, 0, 1)));
        assert_eq!(parse_version("v25.0.1"), Some((25, 0, 1)));
        assert_eq!(parse_version("5.12.0-rc1"), Some((5, 12, 0)));
        assert_eq!(parse_version("4.15"), Some((4, 15, 0)));
        assert_eq!(parse_version("unknown"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_gate_fails_closed_for_unparseable_values() {
        assert!(meets_minimum_version("25.0.1", MINIMUM_DOCKER_VERSION));
        assert!(meets_minimum_version(
            "25.4.0+azure",
            MINIMUM_DOCKER_VERSION
        ));
        assert!(!meets_minimum_version("24.0.9", MINIMUM_DOCKER_VERSION));
        assert!(meets_minimum_version("5.12.0", MINIMUM_KERNEL_VERSION));
        assert!(!meets_minimum_version("5.11.22", MINIMUM_KERNEL_VERSION));
        assert!(!meets_minimum_version("", MINIMUM_KERNEL_VERSION));
    }

    #[test]
    fn rejects_remote_docker_hosts_but_allows_local_sockets() {
        assert!(is_unsupported_docker_host("tcp://10.0.0.5:2375"));
        assert!(is_unsupported_docker_host("ssh://builder@host"));
        assert!(!is_unsupported_docker_host("unix:///var/run/docker.sock"));
        assert!(!is_unsupported_docker_host("/run/user/1000/docker.sock"));
        assert!(!is_unsupported_docker_host(""));
    }

    #[test]
    fn accepts_only_local_unix_endpoints() {
        assert!(is_supported_local_docker_endpoint(
            "unix:///var/run/docker.sock"
        ));
        assert!(is_supported_local_docker_endpoint("/run/docker.sock"));
        assert!(!is_supported_local_docker_endpoint("unix://relative.sock"));
        assert!(!is_supported_local_docker_endpoint("tcp://10.0.0.5:2375"));
        assert!(!is_supported_local_docker_endpoint("ssh://builder@host"));
        assert!(!is_supported_local_docker_endpoint(
            "npipe:////./pipe/docker_engine"
        ));
        assert!(!is_supported_local_docker_endpoint(""));
    }

    #[test]
    fn parses_context_inspect_output() {
        let context = parse_context_endpoint(
            r#"{"Name":"default","Metadata":{},"Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock","SkipTLSVerify":false}},"TLSMaterial":{}}"#,
        )
        .expect("parse context");

        assert_eq!(context.name, "default");
        assert_eq!(
            context.endpoint.as_deref(),
            Some("unix:///var/run/docker.sock")
        );
        assert!(context.verify().is_ok());

        let remote = parse_context_endpoint(
            r#"{"Name":"builder","Endpoints":{"docker":{"Host":"ssh://builder@host"}}}"#,
        )
        .expect("parse context");
        assert_eq!(remote.name, "builder");
        assert!(matches!(
            remote.verify(),
            Err(DockerError::UnsupportedCapability(_))
        ));

        let tcp = parse_context_endpoint(
            r#"{"Name":"ci","Endpoints":{"docker":{"Host":"tcp://10.0.0.5:2375"}}}"#,
        )
        .expect("parse context");
        assert!(matches!(
            tcp.verify(),
            Err(DockerError::UnsupportedCapability(_))
        ));
    }

    #[test]
    fn context_without_endpoint_must_be_the_default_context() {
        let default = DockerContextEndpoint {
            name: "default".to_string(),
            endpoint: None,
        };
        assert!(default.verify().is_ok());

        let named = DockerContextEndpoint {
            name: "builder".to_string(),
            endpoint: None,
        };
        assert!(matches!(
            named.verify(),
            Err(DockerError::UnsupportedCapability(_))
        ));

        let blank = DockerContextEndpoint {
            name: "default".to_string(),
            endpoint: Some("   ".to_string()),
        };
        assert!(blank.verify().is_ok());
    }

    #[test]
    fn explicit_context_name_wins_over_the_cli_current_context() {
        assert_eq!(
            effective_docker_context_name(Some("builder"), Some("default")).as_deref(),
            Some("builder")
        );
        assert_eq!(
            effective_docker_context_name(Some("  "), Some("default")).as_deref(),
            Some("default")
        );
        assert_eq!(
            effective_docker_context_name(None, Some("prod")).as_deref(),
            Some("prod")
        );
        assert_eq!(effective_docker_context_name(None, None), None);
        assert_eq!(effective_docker_context_name(Some(""), Some("")), None);
    }

    #[test]
    fn rejects_a_context_named_like_default_but_pointing_at_a_remote_daemon() {
        // context 名不是授权依据：endpoint 指向远程时，即使名字是 default 也必须拒绝。
        let context = DockerContextEndpoint {
            name: "default".to_string(),
            endpoint: Some("tcp://10.0.0.5:2375".to_string()),
        };

        assert!(matches!(
            verify_context_endpoint(&context),
            Err(DockerError::UnsupportedCapability(_))
        ));
    }
}
