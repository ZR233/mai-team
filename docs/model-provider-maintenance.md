# Provider 与模型目录维护

## 单一来源

mai 不维护内置 provider preset 或模型元数据。canonical 数据位于 PL：

- `pl-model`：`ModelInfo`、能力、参数 wire、token 限制、价格和模型目录。
- `pl-core::ProviderCatalogRegistry`：preset ID、endpoint、凭证描述、协议、连接策略和 catalog 绑定。
- `pl-protocol::ProviderCatalogSnapshot`：Web 与 Flutter 共用的无 secret、版本化 wire 快照。

mai-runtime 的 `MaiConfig` 只保存 provider 实例：ID、preset/catalog 引用、凭证、endpoint
override、连接方式、附加模型和角色路由。mai-store 不包含内置模型构造器。

## 传输边界

协议与连接方式正交：

| Wire protocol | WebSocket | HTTP |
| --- | --- | --- |
| Responses | 支持 | 支持 |
| Chat Completions | 拒绝 | 支持 |

官方 OpenAI preset 使用 Responses，模式顺序为 WebSocket、HTTP，默认 WebSocket。OpenAI 的
HTTP 选项仍调用 Responses HTTP/SSE。自定义 Responses provider 默认 HTTP，可显式选择 WS；
MiMo、DeepSeek、Zhipu 等 Chat-compatible preset 使用 HTTP。运行时不得按厂商 ID 分支，也不得
在 WS 失败时静默降级 HTTP。

同一 preset 可创建多个 provider 实例，唯一性只约束 `ProviderId`。每个实例独立保存 secret、
endpoint、连接模式、附加模型和 routes。

旧配置升级时，只有 endpoint 与 PL preset canonical endpoint 相同的官方 OpenAI 实例采用 WS
默认值。即使实例引用 `openai` preset，只要覆盖了 endpoint（例如 muxai Responses-compatible
网关），也按兼容供应商迁为 HTTP；升级后用户仍可显式切换到 WS。已经保存显式模式的配置原样
保留，运行时不会根据请求结果自动改写模式。

## 产品消费

- `GET /provider-catalog` 返回 canonical snapshot，并以确定性 `revision` 作为 ETag。
- `GET /providers` 返回配置实例、catalog binding、允许模式和服务端解析的 effective models。
- Web 新建表单、模型选择和 reasoning 候选完全读取上述接口，不保留本地 preset/model fallback。
- 更新 API key 时空值表示保留现有 secret；其余 transport、catalog 和 routes 必须原样保存。
- `PUT /providers` 的 `http_headers` 是 write-only 可选字段：省略时，仅在 provider ID、preset、
  protocol 与 endpoint 均未改变的情况下保留现有 headers；显式对象替换 headers，显式空对象
  清空 headers。`GET /providers` 不返回 header 值，避免把可能敏感的供应商配置暴露给 Web。

目录加载失败时 UI 显示错误与重试，不能退回陈旧常量。未知 preset、model、mode 或 icon 必须由
通用组件展示。

## MiMo 资料

MiMo 的两个 preset（API 与 Token Plan）在 PL 中共享 `mimo` catalog，并复用 Chat Completions
HTTP transport，不增加专用 provider enum/runtime。

- 文档索引：<https://platform.xiaomimimo.com/llms.txt>
- 定价、限流和上下文：<https://platform.xiaomimimo.com/static/docs/pricing.md>
- 模型更新：<https://platform.xiaomimimo.com/static/docs/updates/model.md>
- OpenAI-compatible Chat API：<https://platform.xiaomimimo.com/static/docs/api/chat/openai-api.md>
- Token Plan：<https://platform.xiaomimimo.com/token-plan>

## 更新清单

1. 只在 PL canonical catalog 修改 preset/model 元数据和 wire policy。
2. 验证 registry 唯一性、catalog 引用、effective model 冲突和 revision 稳定性。
3. 使用 future provider/model fixture 验证 Web 与 Flutter 无 ID 分支。
4. 重新编译部署 mai；确认 `/provider-catalog` ETag 变化且 `/providers` 自动获得新模型。
5. 对受影响 transport 执行真实 smoke turn，并检查日志不泄漏凭证、不发生静默 fallback。

Responses-compatible 网关需要额外 header 时，把它保存在 provider 实例的 `http_headers`，由同一
`ProviderInfo` 路径同时注入 HTTP 和 WebSocket。不得按 endpoint 或供应商 ID 在 transport 中增加
条件分支。

模型输出 token 字段使用类型化策略 `omit | max_output_tokens | max_completion_tokens | max_tokens`。
产品投影必须同时携带 provider wire protocol：Responses 的 `omit` 不得回退成 Chat Completions 的
`max_tokens`。这类策略属于模型 wire 元数据，不由 Web 根据模型或 provider ID 推断。
