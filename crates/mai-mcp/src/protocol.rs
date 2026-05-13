use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation, PaginatedRequestParams,
    ProtocolVersion,
};
use rmcp::service::RoleClient;
use rmcp::transport::async_rw::AsyncRwTransport;
use serde_json::Value;
use tokio::process::{ChildStdin, ChildStdout};

use crate::error::{McpError, Result};

pub(crate) fn rmcp_transport(
    stdout: ChildStdout,
    stdin: ChildStdin,
) -> AsyncRwTransport<RoleClient, ChildStdout, ChildStdin> {
    AsyncRwTransport::new(stdout, stdin)
}

pub(crate) fn client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("mai-team", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
}

pub(crate) fn call_tool_params(name: &str, arguments: Value) -> Result<CallToolRequestParams> {
    let arguments = match arguments {
        Value::Object(map) => Some(map),
        Value::Null => None,
        other => {
            return Err(McpError::InvalidConfig(
                "tool".to_string(),
                format!("MCP tool arguments must be a JSON object, got {other}"),
            ));
        }
    };
    let mut params = CallToolRequestParams::new(name.to_string());
    params.arguments = arguments;
    Ok(params)
}

pub(crate) fn paginated(cursor: Option<String>) -> Option<PaginatedRequestParams> {
    Some(PaginatedRequestParams::default().with_cursor(cursor))
}
