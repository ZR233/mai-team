use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn turn_loop_has_no_tool_iteration_budget() {
    let mut responses = Vec::new();
    for i in 0..205 {
        responses.push(json!({
            "id": format!("tool_{i}"),
            "output": [{
                "type": "function_call",
                "call_id": format!("call_{i}"),
                "name": "list_agents",
                "arguments": "{}"
            }],
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
        }));
    }
    responses.push(json!({
        "id": "final",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "done after many tools" }]
        }],
        "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
    }));
    let (base_url, requests) = start_mock_responses(responses).await;
    let dir = tempdir().expect("tempdir");
    let store = test_store(&dir).await;
    let mut provider = compact_test_provider(base_url);
    provider.models[0].context_tokens = 1_000_000;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = test_runtime(&dir, Arc::clone(&store)).await;
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "keep going".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");

    assert_eq!(requests.lock().await.len(), 206);
    let (_, messages) = runtime
        .agent_recent_messages(agent_id, 4)
        .await
        .expect("messages");
    assert!(messages.iter().any(|message| {
        message.role == MessageRole::Assistant && message.content == "done after many tools"
    }));
}

#[tokio::test]
async fn model_failure_after_tool_keeps_tool_success_event_separate() {
    let (base_url, _requests) = start_mock_responses(vec![
        json!({
            "id": "first",
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "list_agents",
                "arguments": "{}"
            }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "total_tokens": 12 }
        }),
        json!({ "__close_without_response": true }),
    ])
    .await;
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("runtime.sqlite3");
    let config_path = dir.path().join("config.toml");
    let store = Arc::new(
        ConfigStore::open_with_config_path(&db_path, &config_path)
            .await
            .expect("open store"),
    );
    let mut provider = compact_test_provider(base_url);
    provider.models[0].context_tokens = 1_000_000;
    store
        .save_providers(ProvidersConfigRequest {
            providers: vec![provider],
            default_provider_id: Some("mock".to_string()),
        })
        .await
        .expect("save providers");
    let agent_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    store
        .save_agent(&test_agent_summary(agent_id, Some("container-1")), None)
        .await
        .expect("save agent");
    save_test_session(&store, agent_id, session_id).await;
    let runtime = AgentRuntime::new(
        DockerClient::new("unused"),
        Arc::clone(&store),
        test_runtime_config(&dir, DEFAULT_SIDECAR_IMAGE),
    )
    .await
    .expect("runtime");
    let agent = runtime.agent(agent_id).await.expect("agent");
    *agent.container.write().await = Some(ContainerHandle {
        id: "container-1".to_string(),
        name: "container-1".to_string(),
        image: "unused".to_string(),
    });

    let result = runtime
        .run_turn_inner(
            agent_id,
            session_id,
            Uuid::new_v4(),
            "please list agents".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_err());
    let events = runtime.events.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        ServiceEventKind::ToolCompleted {
            call_id,
            tool_name,
            success: true,
            ..
        } if call_id == "call_1" && tool_name == "list_agents"
    )));
    drop(events);
    let history = store
        .load_agent_history(agent_id, session_id)
        .await
        .expect("history");
    assert!(history.iter().any(|item| {
        pl_protocol::ToolResultMetadata::from_metadata(&item.metadata)
            .is_ok_and(|metadata| metadata.tool_call_id == "call_1")
    }));
}
