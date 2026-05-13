use crate::delivery;
use crate::error::RelayResult;
use crate::rpc::handle_client_request;
use crate::state::{ActiveConnection, AppState};
use axum::extract::ws::{Message, WebSocket};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use mai_protocol::{RelayEnvelope, RelayError, RelayResponse};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub(crate) async fn handle_socket(state: Arc<AppState>, socket: WebSocket) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let Some(Ok(Message::Text(text))) = ws_receiver.next().await else {
        return;
    };
    let Ok(RelayEnvelope::Hello(hello)) = serde_json::from_str::<RelayEnvelope>(&text) else {
        let _ = ws_sender
            .send(Message::Text(
                serde_json::to_string(&RelayEnvelope::Response(RelayResponse {
                    id: "hello".to_string(),
                    result: None,
                    error: Some(RelayError {
                        code: "invalid_hello".to_string(),
                        message: "first message must be hello".to_string(),
                    }),
                }))
                .unwrap()
                .into(),
            ))
            .await;
        return;
    };
    if hello.token != state.token {
        let _ = ws_sender.close().await;
        return;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<RelayEnvelope>();
    {
        let mut connection = state.connection.lock().await;
        *connection = Some(ActiveConnection {
            node_id: hello.node_id.clone(),
            sender: tx.clone(),
            last_heartbeat_at: Utc::now(),
        });
    }
    info!(node_id = %hello.node_id, "relay client connected");
    if let Err(err) = replay_queued(&state, &tx).await {
        warn!("failed to replay queued deliveries: {err}");
    }

    let write_task = tokio::spawn(async move {
        while let Some(envelope) = rx.recv().await {
            match serde_json::to_string(&envelope) {
                Ok(text) => {
                    if ws_sender.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(err) => warn!("failed to serialize relay envelope: {err}"),
            }
        }
    });

    while let Some(message) = ws_receiver.next().await {
        let message = match message {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Pong(_)) => {
                touch_connection(&state, &hello.node_id).await;
                continue;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let Ok(envelope) = serde_json::from_str::<RelayEnvelope>(&message) else {
            warn!("received invalid relay envelope from client");
            continue;
        };
        match envelope {
            RelayEnvelope::Response(response) => {
                if let Some(tx) = state.pending.lock().await.remove(&response.id) {
                    let _ = tx.send(response);
                }
            }
            RelayEnvelope::Ack(ack) => {
                if let Err(err) = delivery::handle_ack(&state, ack).await {
                    warn!("failed to handle ack: {err}");
                }
            }
            RelayEnvelope::Pong { .. } => {
                touch_connection(&state, &hello.node_id).await;
            }
            RelayEnvelope::Request(request) => {
                let state = Arc::clone(&state);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let response = handle_client_request(&state, request).await;
                    let _ = tx.send(RelayEnvelope::Response(response));
                });
            }
            _ => {}
        }
    }

    write_task.abort();
    {
        let mut connection = state.connection.lock().await;
        if connection
            .as_ref()
            .is_some_and(|connection| connection.node_id == hello.node_id)
        {
            *connection = None;
        }
    }
    info!(node_id = %hello.node_id, "relay client disconnected");
}

async fn replay_queued(
    state: &Arc<AppState>,
    tx: &mpsc::UnboundedSender<RelayEnvelope>,
) -> RelayResult<()> {
    for delivery in state.store.list_unacked_deliveries()? {
        tx.send(RelayEnvelope::Event(delivery.into_event())).ok();
    }
    Ok(())
}

async fn touch_connection(state: &Arc<AppState>, node_id: &str) {
    let mut connection = state.connection.lock().await;
    if let Some(connection) = connection.as_mut()
        && connection.node_id == node_id
    {
        connection.last_heartbeat_at = Utc::now();
    }
}
