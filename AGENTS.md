# Project Memory

## Design Principles

- Follow the spirit of Clean Code: small, focused units with one clear reason to change.
- Do not let one large struct or one large `impl` own unrelated responsibilities. Split behavior by role, boundary, and lifecycle instead of letting a facade become the system.
- Prefer high cohesion and low coupling. A public facade may coordinate work, but domain logic, persistence details, validation, conversion, and side effects should live in focused collaborators.
- Choose names that reveal intent, keep functions short enough to understand at a glance, and avoid abstractions that do not remove real complexity.
- When adding behavior, first identify the responsibility it belongs to. If no existing module owns that responsibility cleanly, introduce a small module or type rather than expanding a catch-all structure.
- Treat the workspace as one product that evolves together. Crate-to-crate APIs, server-to-Web UI contracts, and server-to-server messages are internal boundaries that may be changed in one coordinated update.
- Do not preserve backward compatibility for internal project interfaces unless a task explicitly asks for it. Prefer a clean replacement and update all producers, consumers, tests, and documentation in the same change.
- Let design patterns and architecture requirements take priority over compatibility shims. Avoid deprecated aliases, parallel old/new paths, version negotiation, or fallback parsing when the old shape is only used by this project.
- When a boundary becomes awkward, redesign the boundary instead of spreading workarounds across callers. Make ownership, data flow, and lifecycle clear, even if that means changing multiple crates or the Web UI together.

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
