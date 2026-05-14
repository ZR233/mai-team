use std::env;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use mai_model::ModelError;
use mai_protocol::{ModelOutputItem, ModelResponse};

use mai_relay_client::RelayClientConfig;

pub(crate) fn data_dir_path(cli_data_path: Option<PathBuf>) -> Result<PathBuf> {
    Ok(match cli_data_path {
        Some(path) => path,
        None => env::current_dir()?.join(".mai-team"),
    })
}

pub(crate) fn cache_dir_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("cache")
}

pub(crate) fn artifact_files_root(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("artifacts").join("files")
}

pub(crate) fn artifact_index_root(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("artifacts").join("index")
}

pub(crate) fn relay_config_from_env() -> Option<RelayClientConfig> {
    let enabled = env::var("MAI_RELAY_ENABLED")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"));
    if !enabled {
        return None;
    }
    let token = env::var("MAI_RELAY_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        tracing::warn!("MAI_RELAY_ENABLED is set but MAI_RELAY_TOKEN is empty; relay disabled");
        return None;
    }
    let node_id = env::var("MAI_RELAY_NODE_ID").unwrap_or_else(|_| "mai-server".to_string());
    Some(RelayClientConfig {
        url: relay_url_from_env_values(
            env::var("MAI_RELAY_PUBLIC_URL").ok().as_deref(),
            env::var("MAI_RELAY_URL").ok().as_deref(),
        ),
        token,
        node_id,
    })
}

pub(crate) fn relay_url_from_env_values(public_url: Option<&str>, legacy_url: Option<&str>) -> String {
    public_url
        .or(legacy_url)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http://127.0.0.1:8090")
        .trim_end_matches('/')
        .to_string()
}

pub(crate) fn github_callback_page(success: bool, title: &str, message: &str, next: &str) -> Response {
    let status = if success {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let accent = if success { "#0b7a53" } else { "#b42318" };
    let title = html_escape(title);
    let message = html_escape(message);
    let next = html_escape(next);
    let body = format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta http-equiv="refresh" content="2;url={next}">
    <title>{title}</title>
    <style>
      body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #f3f6fa; color: #172033; }}
      main {{ width: min(520px, calc(100vw - 32px)); border: 1px solid #d8e0ea; border-radius: 8px; padding: 28px; background: #fff; box-shadow: 0 16px 36px rgba(22, 32, 51, 0.08); }}
      .mark {{ width: 42px; height: 42px; display: grid; place-items: center; border-radius: 8px; margin-bottom: 18px; background: color-mix(in srgb, {accent} 12%, white); color: {accent}; font-weight: 900; }}
      h1 {{ margin: 0 0 8px; font-size: 22px; }}
      p {{ margin: 0 0 20px; color: #526176; line-height: 1.5; }}
      a {{ color: #1b66d2; font-weight: 800; }}
    </style>
  </head>
  <body>
    <main>
      <div class="mark">{mark}</div>
      <h1>{title}</h1>
      <p>{message}</p>
      <a href="{next}">Return to Mai settings</a>
    </main>
  </body>
</html>"#,
        mark = if success { "OK" } else { "!" }
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .expect("callback response")
}

pub(crate) fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn model_output_preview(response: &ModelResponse) -> String {
    let text = response
        .output
        .iter()
        .filter_map(model_output_item_text)
        .collect::<Vec<_>>()
        .join("\n");
    mai_protocol::preview(&text, 500)
}

pub(crate) fn model_output_item_text(item: &ModelOutputItem) -> Option<String> {
    match item {
        ModelOutputItem::Message { text } => Some(text.clone()),
        ModelOutputItem::AssistantTurn { content, .. } => content.clone(),
        ModelOutputItem::FunctionCall {
            call_id,
            name,
            raw_arguments,
            ..
        } => Some(format!("function_call {name} {call_id}: {raw_arguments}")),
        ModelOutputItem::Other { raw } => Some(raw.to_string()),
    }
}

pub(crate) fn sanitize_provider_test_error(err: &ModelError, api_key: &str) -> String {
    let message = match err {
        ModelError::Request { endpoint, source } => {
            format!("request to {endpoint} failed: {source}")
        }
        ModelError::Api {
            endpoint,
            status,
            body,
        } => {
            let body = mai_protocol::preview(&redact_secret(body, api_key), 1_000);
            format!("request to {endpoint} returned {status}: {body}")
        }
        ModelError::Json(err) => format!("json error: {err}"),
        ModelError::Stream(message) => format!("stream error: {message}"),
        ModelError::Cancelled => "request cancelled".to_string(),
    };
    mai_protocol::preview(&redact_secret(&message, api_key), 1_500)
}

pub(crate) fn redact_secret(value: &str, secret: &str) -> String {
    if secret.trim().is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}

pub(crate) fn bounded_api_limit(limit: Option<usize>, default: usize, max: usize) -> usize {
    limit.unwrap_or(default).clamp(1, max)
}

pub(crate) fn content_disposition_filename(name: &str) -> String {
    let escaped = name
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\r' | '\n' => '_',
            ch if ch.is_control() || !ch.is_ascii() => '_',
            ch => ch,
        })
        .collect::<String>();
    format!("attachment; filename=\"{escaped}\"")
}

#[cfg(test)]
pub(crate) fn data_dir_path_with(
    current_dir: &std::path::Path,
    cli_data_path: Option<PathBuf>,
) -> PathBuf {
    cli_data_path.unwrap_or_else(|| current_dir.join(".mai-team"))
}
