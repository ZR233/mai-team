use std::collections::HashMap;
use std::sync::Arc;

use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde_json::Value;
use tokio::sync::RwLock;

use super::{decode_github_response, github_api_url, github_headers, retry_github_request};
use crate::{Result, RuntimeError};

#[derive(Debug, Clone)]
struct CacheEntry {
    etag: String,
    value: Value,
}

/// GitHub GET 的条件请求缓存。
///
/// 缓存仅保存无 secret 的响应与 ETag；token 只参与哈希键，写入后下一次 GET
/// 仍会向 GitHub 发起条件请求，因此不会把本地 TTL 当作一致性来源。
#[derive(Debug, Clone, Default)]
pub(crate) struct GithubGetCache {
    entries: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl GithubGetCache {
    pub(crate) async fn get(
        &self,
        client: &reqwest::Client,
        api_base_url: &str,
        token: &str,
        path: &str,
    ) -> Result<Value> {
        let key =
            pl_core::canonical_content_hash(format!("{api_base_url}\0{path}\0{token}").as_bytes());
        let cached = self.entries.read().await.get(&key).cloned();
        let (not_modified, etag, value) =
            retry_github_request("read cached project GitHub API", || {
                let cached = cached.clone();
                async move {
                    let mut request = client
                        .get(github_api_url(api_base_url, path))
                        .bearer_auth(token)
                        .headers(github_headers());
                    if let Some(cached) = &cached {
                        request = request.header(IF_NONE_MATCH, &cached.etag);
                    }
                    let response = request.send().await?;
                    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                        return cached
                            .map(|entry| (true, None, entry.value))
                            .ok_or_else(|| {
                                RuntimeError::InvalidInput(
                                    "GitHub returned 304 without a matching cached response"
                                        .to_string(),
                                )
                            });
                    }
                    let etag = response
                        .headers()
                        .get(ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    let value: Value =
                        decode_github_response(response, "read project GitHub API").await?;
                    Ok((false, etag, value))
                }
            })
            .await?;
        if !not_modified {
            match etag {
                Some(etag) => {
                    self.entries.write().await.insert(
                        key,
                        CacheEntry {
                            etag,
                            value: value.clone(),
                        },
                    );
                }
                None => {
                    self.entries.write().await.remove(&key);
                }
            }
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn second_get_sends_etag_and_reuses_not_modified_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock GitHub API");
        let address = listener.local_addr().expect("mock address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in [
                "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: 37\r\nconnection: close\r\n\r\n{\"message\":\"temporarily unavailable\"}",
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\netag: \"v1\"\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"ok\":true}",
                "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).await.expect("read request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).expect("HTTP request is UTF-8"));
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            requests
        });
        let cache = GithubGetCache::default();
        let client = reqwest::Client::new();
        let base_url = format!("http://{address}");

        let first = cache
            .get(&client, &base_url, "secret", "/repos/o/r/pulls/1")
            .await
            .expect("first GitHub GET");
        let second = cache
            .get(&client, &base_url, "secret", "/repos/o/r/pulls/1")
            .await
            .expect("conditional GitHub GET");
        let requests = server.await.expect("mock server joins");

        assert_eq!(first, serde_json::json!({"ok": true}));
        assert_eq!(second, first);
        assert_eq!(requests.len(), 3);
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("if-none-match: \"v1\"")
        );
    }
}
