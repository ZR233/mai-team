use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use mai_protocol::{
    ProjectId, ProjectPullRequestLifecycleState, ProjectPullRequestStateRefreshSummary,
};
use mai_store::{MaiStore, PersistedPullRequestStateObservation};
use reqwest::StatusCode;
use serde_json::{Map, Value, json};

use super::{
    PullRequestStateRefreshCoordinator, decode_github_response, github_api_url, github_headers,
    retry_github_request,
};
use crate::{AgentRuntime, Result, RuntimeError};

const STATE_QUERY_BATCH_SIZE: usize = 50;
const PROJECT_QUERY_CONCURRENCY: usize = 2;
impl AgentRuntime {
    pub async fn refresh_project_pull_request_state(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectPullRequestStateRefreshSummary> {
        self.pull_request_state_refreshes
            .run(project_id, || {
                self.refresh_project_pull_request_state_inner(project_id)
            })
            .await
    }

    async fn refresh_project_pull_request_state_inner(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectPullRequestStateRefreshSummary> {
        let project = self.project(project_id).await?;
        let project = project.summary.read().await.clone();
        let pull_requests = self
            .deps
            .store
            .load_refreshable_project_review_prs(project_id)
            .await?;
        let checked = pull_requests.len();
        if pull_requests.is_empty() {
            return Ok(ProjectPullRequestStateRefreshSummary {
                checked,
                newly_merged: 0,
                newly_closed: 0,
            });
        }
        let token = self.project_git_token(project_id).await?.ok_or_else(|| {
            RuntimeError::InvalidInput("project git account token is not configured".to_string())
        })?;
        refresh_known_pull_requests(
            StateRefreshContext {
                store: &self.deps.store,
                coordinator: &self.pull_request_state_refreshes,
                http: &self.deps.github_http,
                github_api_base_url: &self.github_api_base_url,
                project_id,
                owner: &project.owner,
                repo: &project.repo,
                token: &token,
            },
            pull_requests,
        )
        .await
    }
}

struct StateRefreshContext<'a> {
    store: &'a MaiStore,
    coordinator: &'a PullRequestStateRefreshCoordinator,
    http: &'a reqwest::Client,
    github_api_base_url: &'a str,
    project_id: ProjectId,
    owner: &'a str,
    repo: &'a str,
    token: &'a str,
}

async fn refresh_known_pull_requests(
    context: StateRefreshContext<'_>,
    pull_requests: Vec<u64>,
) -> Result<ProjectPullRequestStateRefreshSummary> {
    let checked = pull_requests.len();
    let batches = pull_requests
        .chunks(STATE_QUERY_BATCH_SIZE)
        .map(<[u64]>::to_vec)
        .collect::<Vec<_>>();
    let results = stream::iter(batches)
        .map(|batch| {
            let token = context.token.to_string();
            let owner = context.owner.to_string();
            let repo = context.repo.to_string();
            async move {
                let _permit = context.coordinator.acquire_github_query().await?;
                query_pull_request_states(
                    context.http,
                    context.github_api_base_url,
                    &token,
                    &owner,
                    &repo,
                    &batch,
                )
                .await
            }
        })
        .buffer_unordered(PROJECT_QUERY_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let detected_at = Utc::now();
    let mut observations = Vec::new();
    let mut incomplete = false;
    for result in results {
        match result {
            Ok(batch) => {
                incomplete |= !batch.complete;
                observations.extend(batch.observations.into_iter().map(
                    |(pr, state, state_changed_at)| PersistedPullRequestStateObservation {
                        pr,
                        state,
                        state_changed_at,
                        detected_at,
                    },
                ));
            }
            Err(error) => {
                tracing::warn!(project_id = %context.project_id, error = %error, "PR state batch refresh failed");
                incomplete = true;
            }
        }
    }
    let saved = context
        .store
        .save_pull_request_state_observations(context.project_id, observations)
        .await?;
    if incomplete {
        return Err(RuntimeError::GithubUnavailable {
            operation: "refresh pull request states".to_string(),
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "one or more GitHub GraphQL batches were incomplete".to_string(),
            retry_after: None,
        });
    }
    Ok(ProjectPullRequestStateRefreshSummary {
        checked,
        newly_merged: saved.newly_merged,
        newly_closed: saved.newly_closed,
    })
}

#[derive(Debug)]
struct PullRequestStateBatch {
    observations: Vec<(u64, ProjectPullRequestLifecycleState, Option<DateTime<Utc>>)>,
    complete: bool,
}

async fn query_pull_request_states(
    http: &reqwest::Client,
    api_base_url: &str,
    token: &str,
    owner: &str,
    repo: &str,
    pull_requests: &[u64],
) -> Result<PullRequestStateBatch> {
    let (query, variables) = state_query(owner, repo, pull_requests)?;
    let url = github_api_url(api_base_url, "/graphql");
    let response = retry_github_request("refresh pull request states", || async {
        let response = http
            .post(&url)
            .bearer_auth(token)
            .headers(github_headers())
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?;
        decode_github_response::<Value>(response, "refresh pull request states").await
    })
    .await?;
    decode_state_query(response, pull_requests)
}

fn state_query(owner: &str, repo: &str, pull_requests: &[u64]) -> Result<(String, Value)> {
    let mut declarations = vec!["$owner: String!".to_string(), "$repo: String!".to_string()];
    let mut selections = Vec::new();
    let mut variables = Map::new();
    variables.insert("owner".to_string(), Value::String(owner.to_string()));
    variables.insert("repo".to_string(), Value::String(repo.to_string()));
    for (index, pr) in pull_requests.iter().enumerate() {
        let pr = i32::try_from(*pr).map_err(|_| {
            RuntimeError::InvalidInput(format!("pull request number {pr} exceeds GraphQL Int"))
        })?;
        declarations.push(format!("$pr_{index}: Int!"));
        selections.push(format!(
            "pr_{index}: pullRequest(number: $pr_{index}) {{ number state mergedAt closedAt }}"
        ));
        variables.insert(format!("pr_{index}"), Value::from(pr));
    }
    Ok((
        format!(
            "query PullRequestLifecycleStates({}) {{ repository(owner: $owner, name: $repo) {{ {} }} }}",
            declarations.join(", "),
            selections.join(" ")
        ),
        Value::Object(variables),
    ))
}

fn decode_state_query(response: Value, pull_requests: &[u64]) -> Result<PullRequestStateBatch> {
    let mut complete = response
        .get("errors")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    let repository = response
        .pointer("/data/repository")
        .and_then(Value::as_object);
    let mut observations = Vec::new();
    for (index, expected_pr) in pull_requests.iter().enumerate() {
        let Some(value) = repository.and_then(|repository| repository.get(&format!("pr_{index}")))
        else {
            complete = false;
            continue;
        };
        let Some(object) = value.as_object() else {
            complete = false;
            continue;
        };
        if object.get("number").and_then(Value::as_u64) != Some(*expected_pr) {
            complete = false;
            continue;
        }
        let (state, timestamp_field) = match object.get("state").and_then(Value::as_str) {
            Some("OPEN") => {
                observations.push((*expected_pr, ProjectPullRequestLifecycleState::Open, None));
                continue;
            }
            Some("MERGED") => (ProjectPullRequestLifecycleState::Merged, "mergedAt"),
            Some("CLOSED") => (ProjectPullRequestLifecycleState::Closed, "closedAt"),
            Some(state) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "pull request #{expected_pr} has unsupported lifecycle state `{state}`"
                )));
            }
            None => {
                return Err(RuntimeError::InvalidInput(format!(
                    "pull request #{expected_pr} is missing lifecycle state"
                )));
            }
        };
        let timestamp = object
            .get(timestamp_field)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "{state} pull request #{expected_pr} is missing {timestamp_field}"
                ))
            })?;
        observations.push((
            *expected_pr,
            state,
            Some(
                DateTime::parse_from_rfc3339(timestamp)
                    .map_err(|error| {
                        RuntimeError::InvalidInput(format!(
                            "{state} pull request #{expected_pr} has invalid {timestamp_field}: {error}"
                        ))
                    })?
                    .with_timezone(&Utc),
            ),
        ));
    }
    Ok(PullRequestStateBatch {
        observations,
        complete,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use mai_protocol::ProjectReviewJobSource;
    use pretty_assertions::assert_eq;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    use super::*;
    use crate::projects::review::job::{NewProjectReviewJob, new_project_review_job};

    #[test]
    fn state_query_uses_typed_variables_for_every_pull_request() {
        let pull_requests = (1..=STATE_QUERY_BATCH_SIZE as u64).collect::<Vec<_>>();
        let (query, variables) = state_query("owner", "repo", &pull_requests).expect("query");
        assert!(query.contains("$pr_49: Int!"));
        assert!(query.contains("pr_49: pullRequest(number: $pr_49)"));
        assert!(query.contains("state mergedAt closedAt"));
        assert_eq!(variables["owner"], "owner");
        assert_eq!(variables["repo"], "repo");
        assert_eq!(variables["pr_49"], 50);
    }

    #[test]
    fn partial_graphql_response_retains_confirmed_merged_results() {
        let merged_at = "2026-08-12T02:00:00Z";
        let batch = decode_state_query(
            json!({
                "data": { "repository": {
                    "pr_0": { "number": 10, "state": "MERGED", "mergedAt": merged_at },
                    "pr_1": { "number": 11, "state": "OPEN", "mergedAt": null }
                }},
                "errors": [{ "message": "one alias failed", "path": ["repository", "pr_2"] }]
            }),
            &[10, 11, 12],
        )
        .expect("partial response");
        assert_eq!(
            batch.observations,
            vec![
                (
                    10,
                    ProjectPullRequestLifecycleState::Merged,
                    Some(
                        DateTime::parse_from_rfc3339(merged_at)
                            .expect("merged at")
                            .with_timezone(&Utc),
                    ),
                ),
                (11, ProjectPullRequestLifecycleState::Open, None),
            ]
        );
        assert!(!batch.complete);
    }

    #[test]
    fn closed_response_becomes_a_persisted_lifecycle_state() {
        let closed_at = "2026-08-12T02:30:00Z";
        let batch = decode_state_query(
            json!({
                "data": { "repository": {
                    "pr_0": {
                        "number": 10,
                        "state": "CLOSED",
                        "mergedAt": null,
                        "closedAt": closed_at
                    }
                }}
            }),
            &[10],
        )
        .expect("closed response");
        assert_eq!(
            batch.observations,
            vec![(
                10,
                ProjectPullRequestLifecycleState::Closed,
                Some(
                    DateTime::parse_from_rfc3339(closed_at)
                        .expect("closed at")
                        .with_timezone(&Utc),
                ),
            )]
        );
        assert!(batch.complete);
    }

    #[test]
    fn merged_response_requires_a_valid_merged_timestamp() {
        for merged_at in [Value::Null, Value::String("not-a-timestamp".to_string())] {
            let error = decode_state_query(
                json!({
                    "data": { "repository": {
                        "pr_0": { "number": 10, "state": "MERGED", "mergedAt": merged_at }
                    }}
                }),
                &[10],
            )
            .expect_err("invalid mergedAt must fail");
            assert!(matches!(error, RuntimeError::InvalidInput(_)));
        }
    }

    #[test]
    fn closed_response_requires_a_valid_closed_timestamp() {
        for closed_at in [Value::Null, Value::String("not-a-timestamp".to_string())] {
            let error = decode_state_query(
                json!({
                    "data": { "repository": {
                        "pr_0": {
                            "number": 10,
                            "state": "CLOSED",
                            "mergedAt": null,
                            "closedAt": closed_at
                        }
                    }}
                }),
                &[10],
            )
            .expect_err("invalid closedAt must fail");
            assert!(matches!(error, RuntimeError::InvalidInput(_)));
        }
    }

    #[test]
    fn unsupported_lifecycle_state_is_rejected() {
        let error = decode_state_query(
            json!({
                "data": { "repository": {
                    "pr_0": { "number": 10, "state": "UNKNOWN" }
                }}
            }),
            &[10],
        )
        .expect_err("unsupported state must fail");
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
    }

    #[test]
    fn mismatched_pull_request_number_marks_batch_incomplete() {
        let batch = decode_state_query(
            json!({
                "data": { "repository": {
                    "pr_0": {
                        "number": 99,
                        "state": "MERGED",
                        "mergedAt": "2026-08-12T02:00:00Z"
                    }
                }}
            }),
            &[10],
        )
        .expect("mismatched response");
        assert_eq!(batch.observations, Vec::new());
        assert!(!batch.complete);
    }

    #[tokio::test]
    async fn refresh_excludes_merged_but_rechecks_closed_and_clears_it_when_reopened() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = test_store(&directory).await;
        let project_id = ProjectId::new_v4();
        save_review_job(&store, project_id, 10).await;
        save_review_job(&store, project_id, 11).await;
        let (base_url, requests) = start_graphql_mock(vec![
            json!({
                "data": { "repository": {
                    "pr_0": {
                        "number": 10,
                        "state": "MERGED",
                        "mergedAt": "2026-08-12T02:00:00Z"
                    },
                    "pr_1": { "number": 11, "state": "OPEN", "mergedAt": null }
                }}
            }),
            json!({
                "data": { "repository": {
                    "pr_0": {
                        "number": 11,
                        "state": "CLOSED",
                        "mergedAt": null,
                        "closedAt": "2026-08-12T03:00:00Z"
                    }
                }}
            }),
            json!({
                "data": { "repository": {
                    "pr_0": { "number": 11, "state": "OPEN", "mergedAt": null, "closedAt": null }
                }}
            }),
        ])
        .await;
        let coordinator = PullRequestStateRefreshCoordinator::default();
        let http = reqwest::Client::new();

        let first_pull_requests = store
            .load_refreshable_project_review_prs(project_id)
            .await
            .expect("initial pull requests");
        let first = refresh_known_pull_requests(
            refresh_context(&store, &coordinator, &http, &base_url, project_id),
            first_pull_requests,
        )
        .await
        .expect("first refresh");
        assert_eq!(
            first,
            ProjectPullRequestStateRefreshSummary {
                checked: 2,
                newly_merged: 1,
                newly_closed: 0,
            }
        );
        assert_eq!(
            store
                .load_project_pull_request_state(project_id, 10)
                .await
                .expect("merged lookup"),
            Some(ProjectPullRequestLifecycleState::Merged)
        );

        let remaining = store
            .load_refreshable_project_review_prs(project_id)
            .await
            .expect("remaining pull requests");
        assert_eq!(remaining, vec![11]);
        let second = refresh_known_pull_requests(
            refresh_context(&store, &coordinator, &http, &base_url, project_id),
            remaining,
        )
        .await
        .expect("second refresh");
        assert_eq!(
            second,
            ProjectPullRequestStateRefreshSummary {
                checked: 1,
                newly_merged: 0,
                newly_closed: 1,
            }
        );
        assert_eq!(
            store
                .load_project_pull_request_state(project_id, 11)
                .await
                .expect("closed lookup"),
            Some(ProjectPullRequestLifecycleState::Closed)
        );
        let closed = store
            .load_refreshable_project_review_prs(project_id)
            .await
            .expect("closed pull request remains refreshable");
        assert_eq!(closed, vec![11]);
        assert_eq!(
            refresh_known_pull_requests(
                refresh_context(&store, &coordinator, &http, &base_url, project_id),
                closed,
            )
            .await
            .expect("reopen refresh"),
            ProjectPullRequestStateRefreshSummary {
                checked: 1,
                newly_merged: 0,
                newly_closed: 0,
            }
        );
        assert_eq!(
            store
                .load_project_pull_request_state(project_id, 11)
                .await
                .expect("reopened lookup"),
            None
        );
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["variables"]["pr_0"], 10);
        assert_eq!(requests[0]["variables"]["pr_1"], 11);
        assert_eq!(requests[1]["variables"]["pr_0"], 11);
        assert!(requests[1]["variables"].get("pr_1").is_none());
        assert_eq!(requests[2]["variables"]["pr_0"], 11);
    }

    #[tokio::test]
    async fn incomplete_refresh_persists_confirmed_merge_before_returning_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = test_store(&directory).await;
        let project_id = ProjectId::new_v4();
        save_review_job(&store, project_id, 20).await;
        save_review_job(&store, project_id, 21).await;
        let (base_url, _requests) = start_graphql_mock(vec![json!({
            "data": { "repository": {
                "pr_0": {
                    "number": 20,
                    "state": "MERGED",
                    "mergedAt": "2026-08-12T03:00:00Z"
                }
            }},
            "errors": [{ "message": "alias failed", "path": ["repository", "pr_1"] }]
        })])
        .await;
        let coordinator = PullRequestStateRefreshCoordinator::default();
        let http = reqwest::Client::new();
        let pull_requests = store
            .load_refreshable_project_review_prs(project_id)
            .await
            .expect("pull requests");

        let error = refresh_known_pull_requests(
            refresh_context(&store, &coordinator, &http, &base_url, project_id),
            pull_requests,
        )
        .await
        .expect_err("incomplete refresh must fail");
        assert!(
            matches!(error, RuntimeError::GithubUnavailable { status, .. } if status == StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            store
                .load_project_pull_request_state(project_id, 20)
                .await
                .expect("confirmed merge"),
            Some(ProjectPullRequestLifecycleState::Merged)
        );
        assert_eq!(
            store
                .load_refreshable_project_review_prs(project_id)
                .await
                .expect("unconfirmed pull requests"),
            vec![21]
        );
    }

    fn refresh_context<'a>(
        store: &'a MaiStore,
        coordinator: &'a PullRequestStateRefreshCoordinator,
        http: &'a reqwest::Client,
        base_url: &'a str,
        project_id: ProjectId,
    ) -> StateRefreshContext<'a> {
        StateRefreshContext {
            store,
            coordinator,
            http,
            github_api_base_url: base_url,
            project_id,
            owner: "owner",
            repo: "repo",
            token: "token",
        }
    }

    async fn test_store(directory: &tempfile::TempDir) -> MaiStore {
        MaiStore::open_with_config_and_artifact_index_path(
            directory.path().join("runtime.sqlite3"),
            directory.path().join("config.toml"),
            directory.path().join("artifacts/index"),
        )
        .await
        .expect("open store")
    }

    async fn save_review_job(store: &MaiStore, project_id: ProjectId, pr: u64) {
        store
            .save_project_review_job(new_project_review_job(NewProjectReviewJob {
                project_id,
                pr,
                head_sha: format!("head-{pr}"),
                source: ProjectReviewJobSource::Manual,
                delivery_id: None,
                reason: "test".to_string(),
            }))
            .await
            .expect("save review job");
    }

    async fn start_graphql_mock(responses: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);
        tokio::spawn(async move {
            let mut responses = VecDeque::from(responses);
            while let Some(response) = responses.pop_front() {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let request = read_http_json_body(&mut stream).await;
                server_requests.lock().await.push(request);
                let body = response.to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(reply.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        (format!("http://{address}"), requests)
    }

    async fn read_http_json_body(stream: &mut tokio::net::TcpStream) -> Value {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 2048];
        loop {
            let read = stream.read(&mut chunk).await.expect("read request");
            assert_ne!(read, 0, "request ended before headers");
            request.extend_from_slice(&chunk[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .expect("content length");
            while request.len() < body_start + content_length {
                let read = stream.read(&mut chunk).await.expect("read request body");
                assert_ne!(read, 0, "request ended before body");
                request.extend_from_slice(&chunk[..read]);
            }
            return serde_json::from_slice(&request[body_start..body_start + content_length])
                .expect("request json");
        }
    }
}
