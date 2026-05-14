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

## Rust 编码规范

### 模块大小

- 目标 Rust 模块在 500 行以下（不含测试）。超过 800 行时，新功能必须放入新模块而非扩展现有文件。
- 提取代码时，将相关测试和文档一并迁移，确保不变量贴近实现代码。
- 避免向已超过 800 行的模块添加不相关功能；高触碰频率的文件尤其需要保持精简。

### 核心 crate 约束

- 抵制向 mai-runtime 添加代码。引入新功能前，先评估：
  1. 是否有其他 crate 更合适（如 mai-agents、mai-tools、mai-mcp）？
  2. 是否该拆出新 crate（如 mai-github、mai-review）？
- mai-runtime 是协调层，不是业务逻辑的归宿。

### Trait 设计

- 新增 trait 必须包含 `///` 文档注释，说明其角色、使用场景和实现约束。
- 使用原生 RPITIT，不使用 `#[async_trait]` 或 `#[allow(async_fn_in_trait)]`。Trait 方法签名应为：
  `fn foo(&self, ...) -> impl std::future::Future<Output = T> + Send;`
- 实现端可使用 `async fn`，但 trait 定义必须显式写出 future 约束。
- 不使用 `#[allow(async_fn_in_trait)]` 作为捷径。

### API 可读性

- 避免在函数签名中使用 `bool` 或裸 `Option<T>` 作为位置参数。优先使用枚举、newtype 或命名构造方法，保持调用端自文档化。
- 不创建仅被调用一次的 helper 方法。
- 模块默认私有，通过 `pub use` 显式导出 crate 的公共 API。
- match 语句应穷尽所有变体，避免使用 `_` 通配符。若枚举可能增长，穷尽 match 能让编译器帮助发现遗漏。

### 测试

- 使用 `pretty_assertions::assert_eq!` 比较完整对象，而非逐字段断言。
- 优先深度相等比较，避免散落的字段级 assert。
- 测试中不修改进程级环境变量，改用依赖注入传入配置。

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
