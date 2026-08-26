pub(crate) mod definitions;
mod names;

#[cfg(test)]
use pl_model::ToolSpec;

pub use names::*;

#[cfg(test)]
pub(crate) fn build_tool_specs() -> Vec<ToolSpec> {
    definitions::builtin_tool_specs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn pure_lang_dependencies_pin_the_verified_runtime_revision() {
        let manifest = include_str!("../../../../Cargo.toml");
        let verified_revision = "989864ca7aba15c00352d9516ed83e02aee2f31a";
        for package in ["pl-core", "pl-model", "pl-protocol", "pl-trace"] {
            let line = manifest
                .lines()
                .find(|line| line.starts_with(&format!("{package} = ")))
                .expect("workspace dependency must exist");
            assert!(
                line.contains("git = \"https://github.com/ZR233/pure-lang.git\"")
                    && line.contains(&format!("rev = \"{verified_revision}\""))
                    && !line.contains("path ="),
                "{package} 必须锁定经过现网数据兼容验证的 pure-lang runtime revision"
            );
        }
    }

    #[test]
    fn builtin_definitions_are_product_tools_only() {
        let tools = build_tool_specs();
        let names = tool_names(&tools);

        assert_eq!(
            names,
            vec![
                TOOL_SAVE_TASK_PLAN,
                TOOL_SUBMIT_REVIEW_RESULT,
                TOOL_SAVE_ARTIFACT,
                TOOL_READ_TOOL_ARTIFACT,
                TOOL_GITHUB_API_REQUEST,
                TOOL_QUEUE_PROJECT_REVIEW_PRS,
            ]
        );
    }

    #[test]
    fn github_request_schema_covers_read_write_without_credentials() {
        let tools = build_tool_specs();
        let request = tools
            .iter()
            .find(|tool| tool.name() == TOOL_GITHUB_API_REQUEST)
            .expect("github_api_request");
        let ToolSpec::Function {
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
            Some(&json!(["object", "null"]))
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
    fn tool_artifact_schema_has_one_unambiguous_range_contract() {
        let tools = build_tool_specs();
        let artifact = tools
            .iter()
            .find(|tool| tool.name() == TOOL_READ_TOOL_ARTIFACT)
            .expect("read_tool_artifact");
        let ToolSpec::Function {
            description,
            input_schema,
            ..
        } = artifact
        else {
            panic!("read_tool_artifact must be a function tool");
        };
        let properties = input_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");

        assert_eq!(
            input_schema.get("required"),
            Some(&json!(["callId", "artifactId", "range"]))
        );
        assert_eq!(
            input_schema.pointer("/$defs/ToolArtifactRange/enum"),
            Some(&json!(["lines", "bytes"]))
        );
        assert!(properties.contains_key("offset"));
        assert!(properties.contains_key("limit"));
        for ambiguous in ["startLine", "maxLines", "startByte", "maxBytes"] {
            assert!(!properties.contains_key(ambiguous), "{ambiguous}");
        }
        for receipt_field in ["outputArtifacts", "call_id", "id"] {
            assert!(
                description.contains(receipt_field),
                "description must identify receipt field {receipt_field}"
            );
        }
        assert!(description.contains("not outputFile"));
    }

    #[test]
    fn product_tool_schemas_use_codex_camel_case_fields() {
        let tools = build_tool_specs();
        let queue = tools
            .iter()
            .find(|tool| tool.name() == TOOL_QUEUE_PROJECT_REVIEW_PRS)
            .expect("queue_project_review_prs");
        let ToolSpec::Function { input_schema, .. } = queue else {
            panic!("queue_project_review_prs must be a function tool");
        };
        let item_properties = input_schema
            .pointer("/$defs/QueueProjectReviewPr/properties")
            .and_then(Value::as_object)
            .expect("queue item properties");

        assert!(item_properties.contains_key("headSha"));
        assert!(!item_properties.contains_key("head_sha"));
    }

    fn tool_names(tools: &[ToolSpec]) -> Vec<&str> {
        tools.iter().map(ToolSpec::name).collect()
    }
}
