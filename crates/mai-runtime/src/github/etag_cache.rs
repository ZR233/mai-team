use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::Arc;

use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde_json::Value;
use tokio::sync::RwLock;

use super::{decode_github_response, github_api_url, github_headers, retry_github_request};
use crate::{Result, RuntimeError};

const MAX_CACHE_ENTRIES: usize = 64;
const MAX_CACHE_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_ENTRY_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct CacheEntry {
    etag: String,
    value: Value,
    body_bytes: usize,
}

impl CacheEntry {
    fn new(etag: String, value: Value) -> Option<Self> {
        let mut counter = ByteCounter::default();
        serde_json::to_writer(&mut counter, &value).ok()?;
        Some(Self {
            etag,
            value,
            body_bytes: counter.bytes,
        })
    }
}

#[derive(Debug, Default)]
struct ByteCounter {
    bytes: usize,
}

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct CacheLimits {
    entries: usize,
    total_body_bytes: usize,
    entry_body_bytes: usize,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            entries: MAX_CACHE_ENTRIES,
            total_body_bytes: MAX_CACHE_BODY_BYTES,
            entry_body_bytes: MAX_CACHE_ENTRY_BODY_BYTES,
        }
    }
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<String, CacheEntry>,
    access_order: VecDeque<String>,
    body_bytes: usize,
}

impl CacheState {
    fn get(&mut self, key: &str) -> Option<CacheEntry> {
        let entry = self.entries.get(key).cloned()?;
        self.access_order.retain(|cached_key| cached_key != key);
        self.access_order.push_back(key.to_string());
        Some(entry)
    }

    fn insert(&mut self, key: String, entry: CacheEntry, limits: CacheLimits) {
        self.remove(&key);
        if limits.entries == 0 || entry.body_bytes > limits.entry_body_bytes {
            return;
        }
        while self.entries.len() >= limits.entries
            || self.body_bytes.saturating_add(entry.body_bytes) > limits.total_body_bytes
        {
            let Some(oldest_key) = self.access_order.pop_front() else {
                break;
            };
            if let Some(oldest) = self.entries.remove(&oldest_key) {
                self.body_bytes = self.body_bytes.saturating_sub(oldest.body_bytes);
            }
        }
        self.body_bytes = self.body_bytes.saturating_add(entry.body_bytes);
        self.access_order.push_back(key.clone());
        self.entries.insert(key, entry);
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.body_bytes = self.body_bytes.saturating_sub(entry.body_bytes);
        }
        self.access_order.retain(|cached_key| cached_key != key);
    }
}

/// GitHub GET 的条件请求缓存。
///
/// 缓存仅保存无 secret 的响应与 ETag；token 只参与哈希键，写入后下一次 GET
/// 仍会向 GitHub 发起条件请求，因此不会把本地 TTL 当作一致性来源。
#[derive(Debug, Clone, Default)]
pub(crate) struct GithubGetCache {
    state: Arc<RwLock<CacheState>>,
    limits: CacheLimits,
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
        let cached = self.state.write().await.get(&key);
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
                    let mut state = self.state.write().await;
                    match CacheEntry::new(etag, value.clone()) {
                        Some(entry) => state.insert(key, entry, self.limits),
                        None => state.remove(&key),
                    }
                }
                None => {
                    self.state.write().await.remove(&key);
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

    #[tokio::test]
    async fn cache_evicts_old_entries_when_capacity_is_reached() {
        const REQUEST_COUNT: usize = MAX_CACHE_ENTRIES + 1;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock GitHub API");
        let address = listener.local_addr().expect("mock address");
        let server = tokio::spawn(async move {
            for index in 0..REQUEST_COUNT {
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
                let body = format!(r#"{{"index":{index}}}"#);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\netag: \"v{index}\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        let cache = GithubGetCache::default();
        let client = reqwest::Client::new();
        let base_url = format!("http://{address}");

        for index in 0..REQUEST_COUNT {
            cache
                .get(
                    &client,
                    &base_url,
                    "secret",
                    &format!("/repos/o/r/pulls/{index}"),
                )
                .await
                .expect("GitHub GET");
        }
        server.await.expect("mock server joins");

        assert!(
            cache.state.read().await.entries.len() <= MAX_CACHE_ENTRIES,
            "GitHub GET cache must stay bounded"
        );
    }

    #[test]
    fn cache_evicts_oldest_body_before_exceeding_byte_budget() {
        let limits = CacheLimits {
            entries: 10,
            total_body_bytes: 20,
            entry_body_bytes: 20,
        };
        let mut state = CacheState::default();

        state.insert(
            "first".to_string(),
            CacheEntry::new("\"v1\"".to_string(), serde_json::json!("1234567890"))
                .expect("serialize first entry"),
            limits,
        );
        state.insert(
            "second".to_string(),
            CacheEntry::new("\"v2\"".to_string(), serde_json::json!("abcdefghij"))
                .expect("serialize second entry"),
            limits,
        );

        assert_eq!(
            (
                state.entries.keys().cloned().collect::<Vec<_>>(),
                state.access_order,
                state.body_bytes,
            ),
            (
                vec!["second".to_string()],
                VecDeque::from(["second".to_string()]),
                12,
            )
        );
    }

    #[test]
    fn cache_skips_a_single_oversized_body() {
        let mut state = CacheState::default();
        state.insert(
            "oversized".to_string(),
            CacheEntry::new(
                "\"large\"".to_string(),
                serde_json::json!("x".repeat(MAX_CACHE_ENTRY_BODY_BYTES + 1)),
            )
            .expect("serialize oversized entry"),
            CacheLimits::default(),
        );

        assert_eq!(
            (
                state.entries.is_empty(),
                state.access_order,
                state.body_bytes
            ),
            (true, VecDeque::new(), 0)
        );
    }
}
