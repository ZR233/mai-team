use pl_model::ToolSchema;

mod definitions;
mod names;
mod schema;

pub use names::*;

pub fn build_tool_schemas() -> Vec<ToolSchema> {
    definitions::builtin_tool_schemas()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn product_tool_api_is_pl_schema_only() {
        let api = include_str!("lib.rs");
        let names = include_str!("names.rs");

        let mcp_tool_type = format!("{}{}", "Mcp", "Tool");
        assert!(
            !api.contains(&mcp_tool_type),
            "mai-tools 只能暴露 mai-team 产品工具 schema，MCP schema 由 pl-core host MCP 工具包构造"
        );
        let mcp_visible_name = format!("{}{}", "model", "_name");
        assert!(
            !api.contains(&mcp_visible_name),
            "mai-tools 不应再拼装 MCP model tool 名称"
        );
        assert!(!api.contains(&format!("{}{}", "route", "_tool")));
        assert!(!api.contains(&format!("{}{}", "Routed", "Tool")));
        assert!(
            !api.contains(&format!("{}{}", "Tool", "Definition")),
            "mai-tools 产品工具 schema 应直接使用 pl_model::ToolSchema"
        );
        assert!(
            !names.contains("TOOL_GIT_SYNC_DEFAULT_BRANCH"),
            "git_sync_default_branch is a pl-core shared git tool"
        );
    }

    #[test]
    fn pure_lang_dependencies_use_shared_branch() {
        let manifest = include_str!("../../../Cargo.toml");
        for package in ["pl-core", "pl-model", "pl-protocol", "pl-trace"] {
            let line = manifest
                .lines()
                .find(|line| line.starts_with(&format!("{package} = ")))
                .expect("workspace dependency must exist");
            assert!(
                line.contains("ssh://git@github.com/ZR233/pure-lang.git"),
                "{package} must use the shared pure-lang git dependency"
            );
            assert!(
                line.contains("branch = \"codex/mai-team-pl-unified-dependency\""),
                "{package} must point at the unified mai-team pure-lang branch"
            );
            assert!(
                !line.contains("path = "),
                "{package} must not use a local path dependency in PR branches"
            );
        }
    }

    #[test]
    fn builtin_definitions_are_product_tools_only() {
        let tools = build_tool_schemas();
        let names = tool_names(&tools);

        assert!(names.contains(&TOOL_GITHUB_API_REQUEST));
        assert!(names.contains(&TOOL_SAVE_ARTIFACT));
        for legacy in [
            pl_core::TOOL_CONTAINER_EXEC,
            pl_core::WorkspaceFileToolKind::ReadFile.name(),
            pl_core::WorkspaceFileToolKind::ListFiles.name(),
            pl_core::WorkspaceFileToolKind::SearchFiles.name(),
            pl_core::WorkspaceFileToolKind::ApplyPatch.name(),
            pl_core::TOOL_SPAWN_AGENT,
            pl_core::TOOL_SEND_INPUT,
            pl_core::TOOL_WAIT_AGENT,
            pl_core::TOOL_LIST_AGENTS,
            pl_core::TOOL_CLOSE_AGENT,
            pl_core::TOOL_RESUME_AGENT,
            "update_todo_list",
            "request_user_input",
            pl_core::TOOL_GIT_STATUS,
            pl_core::TOOL_GIT_PUSH,
            pl_core::TOOL_GIT_SYNC_DEFAULT_BRANCH,
            pl_core::TOOL_LIST_MCP_RESOURCES,
            pl_core::TOOL_LIST_MCP_RESOURCE_TEMPLATES,
            pl_core::TOOL_READ_MCP_RESOURCE,
            "github_api_get",
            "send_message",
            "git_worktree_info",
            "container_cp_upload",
            "container_cp_download",
        ] {
            assert!(
                !names.contains(&legacy),
                "{legacy} must be supplied by pl-core or removed"
            );
        }
    }

    #[test]
    fn github_request_schema_covers_read_write_without_credentials() {
        let tools = build_tool_schemas();
        let request = tools
            .iter()
            .find(|tool| tool.name() == TOOL_GITHUB_API_REQUEST)
            .expect("github_api_request");
        let ToolSchema::Function {
            description,
            input_schema,
            ..
        } = request
        else {
            panic!("github_api_request must be a function tool");
        };
        assert_eq!(
            input_schema.get("required"),
            Some(&json!(["method", "path"]))
        );
        let properties = input_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert_eq!(
            properties.get("body").and_then(|schema| schema.get("type")),
            Some(&json!("object"))
        );
        assert!(description.contains("single POST"));
        assert!(description.contains("event"));
        assert!(description.contains("pending review"));
        for forbidden in ["token", "env", "cwd", "repo_path", "worktree_path"] {
            assert!(
                !properties.contains_key(forbidden),
                "github_api_request exposes forbidden field {forbidden}"
            );
        }
    }

    #[test]
    fn product_tool_schemas_use_codex_camel_case_fields() {
        let tools = build_tool_schemas();
        let queue = tools
            .iter()
            .find(|tool| tool.name() == TOOL_QUEUE_PROJECT_REVIEW_PRS)
            .expect("queue_project_review_prs");
        let ToolSchema::Function { input_schema, .. } = queue else {
            panic!("queue_project_review_prs must be a function tool");
        };
        let item_properties = input_schema
            .pointer("/properties/prs/items/properties")
            .and_then(Value::as_object)
            .expect("queue item properties");

        assert!(item_properties.contains_key("headSha"));
        assert!(!item_properties.contains_key("head_sha"));
    }

    #[test]
    fn product_tool_filtering_uses_pl_core_visibility_set() {
        let tools = pl_core::ToolVisibilitySet::from_tool_names([TOOL_SAVE_ARTIFACT])
            .filter_schemas(build_tool_schemas());
        assert_eq!(tool_names(&tools), vec![TOOL_SAVE_ARTIFACT]);
    }

    fn tool_names(tools: &[ToolSchema]) -> Vec<&str> {
        tools.iter().map(ToolSchema::name).collect()
    }
}
