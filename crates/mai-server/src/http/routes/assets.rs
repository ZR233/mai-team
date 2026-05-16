use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/static"]
struct StaticAssets;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpaFallback {
    Enabled,
}

impl SpaFallback {
    fn is_enabled(self) -> bool {
        match self {
            Self::Enabled => true,
        }
    }
}

pub(crate) async fn index() -> Response {
    embedded_asset_response("index.html", SpaFallback::Enabled)
}

pub(crate) async fn static_fallback(uri: Uri) -> Response {
    embedded_asset_response(uri.path().trim_start_matches('/'), SpaFallback::Enabled)
}

fn embedded_asset_response(path: &str, fallback: SpaFallback) -> Response {
    let asset_path = if path.is_empty() { "index.html" } else { path };
    let (served_path, asset) = match StaticAssets::get(asset_path) {
        Some(asset) => (asset_path, asset),
        None if fallback.is_enabled() && !asset_path.contains('.') => {
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

    let mut response = Response::new(Body::from(asset.data.into_owned()));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    *response.status_mut() = StatusCode::OK;
    response
}
