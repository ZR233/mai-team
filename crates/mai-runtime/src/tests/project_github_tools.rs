use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn github_api_request_runs_via_gh_sidecar_without_token_leak() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
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
    seed_project_workspace_volumes(&dir, project_id, &[(maintainer_id, "planner")]);
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let result = runtime
        .execute_tool_for_test(
            maintainer_id,
            "github_api_request",
            json!({
                "method": "POST",
                "path": "/repos/owner/repo/pulls/123/reviews",
                "body": {
                    "event": "COMMENT",
                    "body": "Looks good."
                }
            }),
        )
        .await
        .expect("github api request");

    assert!(result.success);
    assert_eq!(
        serde_json::from_str::<Value>(&result.output).expect("json output"),
        json!({"ok": true})
    );
    let docker_log = fake_docker_log(&dir);
    assert!(docker_log.contains("sidecar-gh-api"));
    assert!(docker_log.contains("--network host"));
    assert!(docker_log.contains("--method POST"));
    assert!(docker_log.contains("MAI_GH_API_BODY"));
    assert!(docker_log.contains("-e GH_TOKEN"));
    assert!(docker_log.contains("token-present"));
    assert!(!docker_log.contains("secret-token"));
}

#[tokio::test]
async fn reviewer_github_api_request_rejects_pending_pr_review_creation() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut reviewer = test_agent_summary(reviewer_id, Some("reviewer-container"));
    reviewer.project_id = Some(project_id);
    reviewer.role = Some(AgentRole::Reviewer);
    save_agent_with_session(&store, &reviewer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            reviewer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    seed_project_workspace_volumes(&dir, project_id, &[(reviewer_id, "reviewer")]);
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let err = runtime
        .execute_tool_for_test(
            reviewer_id,
            "github_api_request",
            json!({
                "method": "POST",
                "path": "/repos/owner/repo/pulls/123/reviews",
                "body": {
                    "event": "APPROVE"
                }
            }),
        )
        .await
        .expect_err("pending review creation should be rejected before gh sidecar");

    assert!(err.to_string().contains("non-empty `body`"));
    assert!(!fake_docker_log(&dir).contains("sidecar-gh-api"));
}

#[tokio::test]
async fn reviewer_github_api_request_rejects_pending_review_event_submission() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut reviewer = test_agent_summary(reviewer_id, Some("reviewer-container"));
    reviewer.project_id = Some(project_id);
    reviewer.role = Some(AgentRole::Reviewer);
    save_agent_with_session(&store, &reviewer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            reviewer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    seed_project_workspace_volumes(&dir, project_id, &[(reviewer_id, "reviewer")]);
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let err = runtime
        .execute_tool_for_test(
            reviewer_id,
            "github_api_request",
            json!({
                "method": "POST",
                "path": "/repos/owner/repo/pulls/123/reviews/456/events",
                "body": {
                    "event": "APPROVE",
                    "body": "Looks good after validation."
                }
            }),
        )
        .await
        .expect_err("pending review event submission should be rejected before gh sidecar");

    assert!(err.to_string().contains("one single POST"));
    assert!(!fake_docker_log(&dir).contains("sidecar-gh-api"));
}

#[tokio::test]
async fn reviewer_github_api_request_rejects_direct_inline_comment_creation() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
    let project_id = Uuid::new_v4();
    let reviewer_id = Uuid::new_v4();
    let mut reviewer = test_agent_summary(reviewer_id, Some("reviewer-container"));
    reviewer.project_id = Some(project_id);
    reviewer.role = Some(AgentRole::Reviewer);
    save_agent_with_session(&store, &reviewer).await;
    store
        .save_project(&ready_test_project_summary(
            project_id,
            reviewer_id,
            "account-1",
        ))
        .await
        .expect("save project");
    seed_project_workspace_volumes(&dir, project_id, &[(reviewer_id, "reviewer")]);
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let err = runtime
        .execute_tool_for_test(
            reviewer_id,
            "github_api_request",
            json!({
                "method": "POST",
                "path": "/repos/owner/repo/pulls/123/comments",
                "body": {
                    "path": "src/lib.rs",
                    "line": 12,
                    "side": "RIGHT",
                    "body": "Please cover this edge case."
                }
            }),
        )
        .await
        .expect_err("direct inline review comment creation should be rejected before gh sidecar");

    assert!(err.to_string().contains("one single POST"));
    assert!(err.to_string().contains("comments` array"));
    assert!(!fake_docker_log(&dir).contains("sidecar-gh-api"));
}

#[tokio::test]
async fn github_api_request_accepts_json_string_body() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
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
    seed_project_workspace_volumes(&dir, project_id, &[(maintainer_id, "planner")]);
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;

    let result = runtime
        .execute_tool_for_test(
            maintainer_id,
            "github_api_request",
            json!({
                "method": "POST",
                "path": "/repos/owner/repo/pulls/123/reviews",
                "body": r#"{"event":"COMMENT","body":"Looks good."}"#
            }),
        )
        .await
        .expect("github api request");

    assert!(result.success);
    let docker_log = fake_docker_log(&dir);
    assert!(docker_log.contains("sidecar-gh-api"));
    assert!(docker_log.contains("MAI_GH_API_BODY"));
    assert!(docker_log.contains(r#"gh-body={"body":"Looks good.","event":"COMMENT"}"#));
    assert!(!docker_log.contains(r#"gh-body="{\"event\":\"COMMENT\""#));
}

#[tokio::test]
async fn github_api_request_uses_standard_token_env_with_custom_api_base_url() {
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    store
        .upsert_git_account(GitAccountRequest {
            id: Some("account-1".to_string()),
            label: "GitHub".to_string(),
            token: Some("secret-token".to_string()),
            is_default: true,
            ..Default::default()
        })
        .await
        .expect("save account");
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
    seed_project_workspace_volumes(&dir, project_id, &[(maintainer_id, "planner")]);
    let runtime = test_runtime_with_github_api(
        &dir,
        Arc::clone(&store),
        "https://ghe.example.com/api/v3".to_string(),
    )
    .await;

    let result = runtime
        .execute_tool_for_test(
            maintainer_id,
            "github_api_request",
            json!({
                "method": "POST",
                "path": "/repos/owner/repo/pulls/123/reviews",
                "body": {
                    "event": "COMMENT",
                    "body": "Looks good."
                }
            }),
        )
        .await
        .expect("github api request");

    assert!(result.success);
    assert_eq!(
        serde_json::from_str::<Value>(&result.output).expect("json output"),
        json!({"ok": true})
    );
    let docker_log = fake_docker_log(&dir);
    assert!(docker_log.contains("sidecar-gh-api"));
    assert!(docker_log.contains("--network host"));
    assert!(docker_log.contains("-e GH_TOKEN"));
    assert!(docker_log.contains("/repos/owner/repo/pulls/123/reviews"));
    assert!(docker_log.contains("token-present"));
    assert!(!docker_log.contains("secret-token"));
}
