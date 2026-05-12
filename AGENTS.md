# Project Memory

## Viewing Runtime Logs

- Server process logs are emitted through `tracing_subscriber` to stdout/stderr. When starting the server locally, use:

```bash
RUST_LOG=mai_server=info,mai_runtime=debug,mai_model=debug,tower_http=info cargo run -p mai-server
```

- If the server is already running in a terminal, inspect that terminal first. The default filter is `mai_server=info,mai_runtime=info,tower_http=info` unless `RUST_LOG` is set.
- Agent runtime logs are also persisted in the local store and exposed by:

```bash
curl 'http://127.0.0.1:8080/agents/{agent_id}/logs?limit=100'
```

- Useful filters: `session_id`, `turn_id`, `level`, `category`, `since`, `until`, `offset`, `limit`. Examples:

```bash
curl 'http://127.0.0.1:8080/agents/{agent_id}/logs?category=model&limit=20'
curl 'http://127.0.0.1:8080/agents/{agent_id}/logs?level=warn&limit=50'
```

- The default database path is `~/.mai-team/mai-team.sqlite3` unless `MAI_DB_PATH` is set. The default config path is `~/.mai-team/config.toml` unless `MAI_CONFIG_PATH` is set.
