use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

pub(crate) fn callback_page(success: bool, title: &str, message: &str) -> Response {
    let status = if success {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let accent = if success { "#0b7a53" } else { "#b42318" };
    let title = html_escape(title);
    let message = html_escape(message);
    (
        status,
        Html(format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
             <body style=\"font-family: system-ui, sans-serif; margin: 3rem; line-height: 1.5\">\
             <h1 style=\"color:{accent}\">{title}</h1><p>{message}</p></body>"
        )),
    )
        .into_response()
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
