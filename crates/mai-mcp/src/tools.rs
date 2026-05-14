use std::collections::{BTreeMap, BTreeSet};

use mai_protocol::McpServerConfig;
use rmcp::model::{ListToolsResult, Tool};
use serde_json::{Map, Value, json};

use crate::naming::{fnv1a_hex, model_tool_name};
use crate::types::McpTool;

pub(crate) fn parse_tools_result(
    server: &str,
    config: &McpServerConfig,
    result: ListToolsResult,
) -> Vec<McpTool> {
    let enabled = config
        .enabled_tools
        .as_ref()
        .map(|tools| tools.iter().cloned().collect::<BTreeSet<_>>());
    let disabled = config
        .disabled_tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    result
        .tools
        .into_iter()
        .filter(|tool| {
            enabled
                .as_ref()
                .is_none_or(|tools| tools.contains(tool.name.as_ref()))
                && !disabled.contains(tool.name.as_ref())
        })
        .map(|tool| parse_tool(server, tool))
        .collect()
}

fn parse_tool(server: &str, tool: Tool) -> McpTool {
    let name = tool.name.to_string();
    let description = tool.description.unwrap_or_default().to_string();
    let input_schema = normalize_input_schema(Value::Object(tool.input_schema.as_ref().clone()));
    let output_schema = tool
        .output_schema
        .map(|schema| Value::Object(schema.as_ref().clone()));
    McpTool {
        model_name: model_tool_name(server, &name),
        server: server.to_string(),
        name,
        description,
        input_schema,
        output_schema,
    }
}

fn normalize_input_schema(mut schema: Value) -> Value {
    if !schema.is_object() {
        schema = json!({ "type": "object", "properties": {} });
    }
    if let Value::Object(map) = &mut schema {
        map.entry("type".to_string())
            .or_insert_with(|| Value::String("object".to_string()));
        let missing_properties = map
            .get("properties")
            .is_none_or(|properties| properties.is_null());
        if missing_properties {
            map.insert("properties".to_string(), Value::Object(Map::new()));
        }
    }
    schema
}

pub(crate) fn collision_safe_tool_name(
    existing: &BTreeMap<String, McpTool>,
    tool: &McpTool,
) -> String {
    if !existing.contains_key(&tool.model_name) {
        return tool.model_name.clone();
    }
    let suffix = fnv1a_hex(&format!("{}::{}", tool.server, tool.name));
    let keep = 64usize.saturating_sub(suffix.len() + 2);
    let prefix = if tool.model_name.len() > keep {
        &tool.model_name[..keep]
    } else {
        &tool.model_name
    };
    let mut candidate = format!("{prefix}__{suffix}");
    let mut index = 2usize;
    while existing.contains_key(&candidate) {
        let extra = format!("_{index}");
        let keep = 64usize.saturating_sub(suffix.len() + extra.len() + 2);
        let prefix = if tool.model_name.len() > keep {
            &tool.model_name[..keep]
        } else {
            &tool.model_name
        };
        candidate = format!("{prefix}__{suffix}{extra}");
        index += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crate::naming::model_tool_name;

    #[test]
    fn tool_schema_gets_properties() {
        let tool = Tool::new_with_raw(
            "echo",
            None,
            Map::from_iter([("type".to_string(), json!("object"))]),
        );
        let tool = parse_tool("demo", tool);
        assert!(tool.input_schema.get("properties").is_some());
    }

    #[test]
    fn parse_tools_applies_allow_and_deny_filters() {
        let config = McpServerConfig {
            enabled_tools: Some(vec!["keep".to_string(), "drop".to_string()]),
            disabled_tools: vec!["drop".to_string()],
            ..Default::default()
        };
        let result = ListToolsResult {
            tools: vec![
                Tool::new_with_raw("keep", Some(Cow::Borrowed("")), Map::new()),
                Tool::new_with_raw("drop", Some(Cow::Borrowed("")), Map::new()),
                Tool::new_with_raw("other", Some(Cow::Borrowed("")), Map::new()),
            ],
            ..Default::default()
        };

        let tools = parse_tools_result("demo", &config, result);

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "keep");
    }

    #[test]
    fn collision_safe_tool_names_preserve_both_tools() {
        let first = McpTool {
            server: "a.b".to_string(),
            name: "read file".to_string(),
            model_name: model_tool_name("a.b", "read file"),
            description: String::new(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: None,
        };
        let second = McpTool {
            server: "a_b".to_string(),
            name: "read_file".to_string(),
            model_name: model_tool_name("a_b", "read_file"),
            description: String::new(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: None,
        };
        let mut existing = BTreeMap::new();
        existing.insert(first.model_name.clone(), first);

        let name = collision_safe_tool_name(&existing, &second);

        assert_ne!(name, second.model_name);
        assert!(name.starts_with("mcp__a_b__read_file__"));
        assert!(name.len() <= 64);
    }
}
