use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn project_agent_without_discovered_mcp_tools_has_no_static_fallback() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");

    let tools = runtime.agent_mcp_tools(&maintainer_record).await;

    assert!(tools.is_empty());
    let visible =
        turn::tool_visibility::visible_tool_names(&runtime.state, &maintainer_record, &tools).await;
    assert!(!visible.contains("mcp__github__create_pull_request_review"));
    assert!(!visible.contains("mcp__github__pull_request_review_write"));
    assert!(!visible.contains("mcp__git__git_status"));
}

#[tokio::test]
async fn project_mcp_tools_are_not_visible_even_if_cached() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let project_id = Uuid::new_v4();
    let maintainer_id = Uuid::new_v4();
    let mut maintainer = test_agent_summary(maintainer_id, Some("maintainer-container"));
    maintainer.project_id = Some(project_id);
    save_agent_with_session(&store, &maintainer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            maintainer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let discovered = vec![
        test_mcp_tool("github", "pull_request_review_write"),
        test_mcp_tool("git", "git_diff_unstaged"),
    ];
    runtime.state.project_mcp_managers.write().await.insert(
        project_id,
        projects::mcp::ProjectMcpManagerHandle::without_token_expiry(Arc::new(
            McpAgentManager::from_tools_for_test(discovered.clone()),
        )),
    );
    let maintainer_record = runtime.agent(maintainer_id).await.expect("maintainer");

    let tools = runtime.agent_mcp_tools(&maintainer_record).await;

    let names = tools
        .iter()
        .map(|tool| tool.model_name.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(names, HashSet::new());
    let visible =
        turn::tool_visibility::visible_tool_names(&runtime.state, &maintainer_record, &tools).await;
    assert!(!visible.contains("mcp__git__git_diff_unstaged"));
    assert!(!visible.contains("mcp__github__pull_request_review_write"));
    assert!(!visible.contains("mcp__github__create_pull_request_review"));
    assert!(!visible.contains("mcp__git__git_status"));
}

#[test]
fn project_mcp_configs_do_not_start_github_mcp_server() {
    let configs = projects::mcp::project_mcp_configs("secret-token");
    assert!(configs.is_empty());
    assert!(!format!("{configs:?}").contains("secret-token"));
    assert!(!format!("{configs:?}").contains("github-mcp-server"));
    assert!(!configs.contains_key("git"));
}

#[tokio::test]
async fn delete_project_removes_project_mcp_manager() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![test_provider()],
            default_provider_id: Some("openai".to_string()),
        })
        .await
        .expect("save providers");
    let project_id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let mut agent = test_agent_summary(agent_id, Some("maintainer-container"));
    agent.project_id = Some(project_id);
    save_agent_with_session(&store, &agent).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            agent_id,
            "account-1",
        ))
        .await
        .expect("save project");
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    runtime.state.project_mcp_managers.write().await.insert(
        project_id,
        projects::mcp::ProjectMcpManagerHandle::without_token_expiry(Arc::new(
            McpAgentManager::from_tools_for_test(vec![test_mcp_tool("github", "get_me")]),
        )),
    );

    runtime.delete_project(project_id).await.expect("delete");

    assert!(
        !runtime
            .state
            .project_mcp_managers
            .read()
            .await
            .contains_key(&project_id)
    );
}
