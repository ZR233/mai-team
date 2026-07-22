use pl_model::ToolSchema;

mod definitions;
mod names;

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
        let api = include_str!("product_tool_schemas.rs");
        let names = include_str!("product_tool_schemas/names.rs");

        let mcp_tool_type = format!("{}{}", "Mcp", "Tool");
        assert!(
            !api.contains(&mcp_tool_type),
            "product_tool_schemas 只能暴露 mai-team 产品工具 schema，MCP schema 由 pl-core host MCP 工具包构造"
        );
        let mcp_visible_name = format!("{}{}", "model", "_name");
        assert!(
            !api.contains(&mcp_visible_name),
            "product_tool_schemas 不应再拼装 MCP model tool 名称"
        );
        assert!(!api.contains(&format!("{}{}", "route", "_tool")));
        assert!(!api.contains(&format!("{}{}", "Routed", "Tool")));
        assert!(
            !api.contains(&format!("{}{}", "Tool", "Definition")),
            "product_tool_schemas 产品工具 schema 应直接使用 pl_model::ToolSchema"
        );
        assert!(
            !names.contains("TOOL_GIT_SYNC_DEFAULT_BRANCH"),
            "git_sync_default_branch is a pl-core shared git tool"
        );
    }

    #[test]
    fn pure_lang_dependencies_use_upstream_main() {
        let manifest = include_str!("../../../../Cargo.toml");
        for package in ["pl-core", "pl-model", "pl-protocol", "pl-trace"] {
            let line = manifest
                .lines()
                .find(|line| line.starts_with(&format!("{package} = ")))
                .expect("workspace dependency must exist");
            assert!(
                line.contains("git = \"ssh://git@github.com/ZR233/pure-lang.git\"")
                    && line.contains("branch = \"main\"")
                    && !line.contains("path ="),
                "{package} must use the pure-lang upstream main branch"
            );
        }
    }

    #[test]
    fn pure_lang_shared_tools_include_session_note_contract() {
        let names = pl_core::shared_tool_names(
            pl_core::SharedToolSchemaOptions::from_capabilities(
                &pl_core::ToolCapabilityConfig::container_workspace(),
            )
            .with_plan_exit(false),
        );

        for name in [
            pl_core::TOOL_READ_SESSION_NOTE,
            pl_core::TOOL_SEARCH_SESSION_NOTE,
            pl_core::TOOL_WRITE_SESSION_NOTE,
            pl_core::TOOL_APPLY_SESSION_NOTE_PATCH,
        ] {
            assert!(names.iter().any(|candidate| candidate == name), "{name}");
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
            "spawn_agent",
            "send_input",
            "wait_agent",
            "list_agents",
            "close_agent",
            "resume_agent",
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
    fn product_tool_schemas_use_pl_core_schema_helpers() {
        let definitions = [
            include_str!("product_tool_schemas/definitions/workflow.rs"),
            include_str!("product_tool_schemas/definitions/github.rs"),
            include_str!("product_tool_schemas/definitions/review.rs"),
        ]
        .join("\n");

        assert!(
            definitions.contains("function_tool_schema(")
                && definitions.contains("ToolInputSchemaField::required"),
            "product_tool_schemas 产品工具 schema 应通过 pl-core 统一 helper 构造"
        );
        for forbidden in [
            "ToolSchema::function",
            "crate::schema::object_schema",
            "object_schema(vec!",
        ] {
            assert!(
                !definitions.contains(forbidden),
                "product_tool_schemas 不应保留本地工具 schema 构造 `{forbidden}`"
            );
        }
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
