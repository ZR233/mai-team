use axum::extract::Request;
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::Response;

use super::routes;

/// 对与 JSON API 共用路径前缀的 React 深链执行 HTML 内容协商。
///
/// 浏览器文档导航会携带 `Accept: text/html`，而 Web API client 明确请求 JSON。
/// 只拦截已知 UI route，避免影响 GitHub callback、下载和普通 API 客户端。
pub(crate) async fn serve_spa_navigation(request: Request, next: Next) -> Response {
    if is_spa_navigation(&request) {
        routes::assets::index().await
    } else {
        next.run(request).await
    }
}

fn is_spa_navigation(request: &Request) -> bool {
    request.method() == Method::GET
        && accepts_html(request)
        && matches_spa_route(request.uri().path())
}

fn accepts_html(request: &Request) -> bool {
    request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|media_type| media_type.starts_with("text/html"))
        })
}

fn matches_spa_route(path: &str) -> bool {
    ["/chat", "/tasks", "/projects", "/providers", "/settings"]
        .into_iter()
        .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn html_navigation_wins_only_for_ui_routes() {
        let cases = [
            ("/projects/project-id", "text/html", true),
            ("/tasks/task-id", "text/html,application/xhtml+xml", true),
            ("/settings/web-search", "text/html", true),
            ("/settings/web-search", "application/json", false),
            ("/projects/project-id", "*/*", false),
            ("/github/app-installation/callback", "text/html", false),
        ];

        let actual = cases
            .into_iter()
            .map(|(path, accept, _)| {
                let request = Request::builder()
                    .uri(path)
                    .header(header::ACCEPT, accept)
                    .body(Body::empty())
                    .unwrap();
                is_spa_navigation(&request)
            })
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            cases
                .into_iter()
                .map(|(_, _, expected)| expected)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn non_get_requests_are_never_spa_navigation() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/projects/project-id")
            .header(header::ACCEPT, "text/html")
            .body(Body::empty())
            .unwrap();

        assert!(!is_spa_navigation(&request));
    }
}
