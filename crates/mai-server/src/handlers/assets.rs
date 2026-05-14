use std::io;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::Mutex as StdMutex;

use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_embed::RustEmbed;
use serde_json::json;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/static"]
struct StaticAssets;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/system-skills"]
struct EmbeddedSystemSkills;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/system-agents"]
struct EmbeddedSystemAgents;

static EMBEDDED_RESOURCE_RELEASE_LOCK: StdMutex<()> = StdMutex::new(());

pub(crate) async fn index() -> Response {
    embedded_asset_response("index.html", true)
}

pub(crate) async fn static_fallback(uri: Uri) -> Response {
    embedded_asset_response(uri.path().trim_start_matches('/'), true)
}

pub(crate) async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

fn embedded_asset_response(path: &str, fallback_index: bool) -> Response {
    let asset_path = if path.is_empty() { "index.html" } else { path };
    let (served_path, asset) = match StaticAssets::get(asset_path) {
        Some(asset) => (asset_path, asset),
        None if fallback_index && !asset_path.contains('.') => {
            match StaticAssets::get("index.html") {
                Some(asset) => ("index.html", asset),
                None => {
                    return (StatusCode::NOT_FOUND, "embedded index.html not found")
                        .into_response();
                }
            }
        }
        None => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let content_type = mime_guess::from_path(served_path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(asset.data.into_owned()))
        .expect("embedded static response")
}

pub(crate) fn system_skills_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("system-skills")
}

pub(crate) fn system_agents_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("system-agents")
}

pub(crate) fn release_embedded_system_skills(target_dir: &std::path::Path) -> io::Result<()> {
    release_embedded_resources::<EmbeddedSystemSkills>(
        target_dir,
        safe_system_resource_target,
        "system-skills",
    )
}

pub(crate) fn release_embedded_system_agents(target_dir: &std::path::Path) -> io::Result<()> {
    release_embedded_resources::<EmbeddedSystemAgents>(
        target_dir,
        safe_system_resource_target,
        "system-agents",
    )
}

pub(crate) fn release_embedded_resources<E>(
    target_dir: &std::path::Path,
    is_safe_target: fn(&std::path::Path) -> bool,
    out_dir_name: &str,
) -> io::Result<()>
where
    E: RustEmbed,
{
    let _guard = EMBEDDED_RESOURCE_RELEASE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !is_safe_target(target_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe system resource target: {}", target_dir.display()),
        ));
    }
    if target_dir.exists() {
        std::fs::remove_dir_all(target_dir)?;
    }
    std::fs::create_dir_all(target_dir)?;
    for path in E::iter() {
        let path = path.as_ref();
        let Some(relative) = embedded_system_resource_relative_path(path, out_dir_name) else {
            continue;
        };
        let target = target_dir.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(asset) = E::get(path) {
            std::fs::write(target, asset.data.as_ref())?;
        }
    }
    Ok(())
}

pub(crate) fn safe_system_resource_target(path: &std::path::Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    !matches!(
        path.components().next_back(),
        None | Some(Component::RootDir | Component::Prefix(_))
    )
}

pub(crate) fn embedded_system_resource_relative_path(path: &str, out_dir_name: &str) -> Option<PathBuf> {
    let path = FsPath::new(path);
    let relative = if path.is_absolute() {
        path.strip_prefix(FsPath::new(env!("OUT_DIR")).join(out_dir_name))
            .ok()?
    } else {
        path
    };
    let relative = relative.strip_prefix(out_dir_name).unwrap_or(relative);
    safe_embedded_relative_path_from_path(relative)
}

pub(crate) fn safe_embedded_relative_path_from_path(path: &FsPath) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

#[cfg(test)]
pub(crate) fn embedded_system_skill_relative_path(path: &str) -> Option<PathBuf> {
    embedded_system_resource_relative_path(path, "system-skills")
}

#[cfg(test)]
pub(crate) fn embedded_system_agent_relative_path(path: &str) -> Option<PathBuf> {
    embedded_system_resource_relative_path(path, "system-agents")
}

#[cfg(test)]
pub(crate) fn safe_embedded_relative_path(path: &str) -> Option<PathBuf> {
    safe_embedded_relative_path_from_path(FsPath::new(path))
}
