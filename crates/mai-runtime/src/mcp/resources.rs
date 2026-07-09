use rmcp::model::{ListResourceTemplatesResult, ListResourcesResult};
use serde_json::{Value, json};

pub(crate) fn list_resources_value(server: Option<&str>, result: ListResourcesResult) -> Value {
    let resources = result
        .resources
        .into_iter()
        .map(|resource| {
            let value = serde_json::to_value(resource).unwrap_or(Value::Null);
            match server {
                Some(server) => with_server(server, value),
                None => value,
            }
        })
        .collect::<Vec<_>>();
    json!({
        "server": server,
        "resources": resources,
        "nextCursor": result.next_cursor,
    })
}

pub(crate) fn list_resource_templates_value(
    server: Option<&str>,
    result: ListResourceTemplatesResult,
) -> Value {
    let resource_templates = result
        .resource_templates
        .into_iter()
        .map(|template| {
            let value = serde_json::to_value(template).unwrap_or(Value::Null);
            match server {
                Some(server) => with_server(server, value),
                None => value,
            }
        })
        .collect::<Vec<_>>();
    json!({
        "server": server,
        "resourceTemplates": resource_templates,
        "nextCursor": result.next_cursor,
    })
}

pub(crate) fn with_server(server: &str, value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            map.insert("server".to_string(), Value::String(server.to_string()));
            Value::Object(map)
        }
        other => json!({ "server": server, "value": other }),
    }
}
