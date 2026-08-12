# mai 定制宿主与 PL 框架边界

## 目标

mai 是基于 `pl-core` 的定制化 agent 产品。`pl-core` 唯一拥有 agent/thread/turn 执行
状态机；mai 只负责产品配置、容器、项目、任务、GitHub、review、产品工具以及协议投影。

## 运行时接入

`MaiAgentHost` 在 `mai-runtime::agent_host` 中实现：

- repository：将 `ThreadActorState`、mailbox、Turn、Item、interaction、transcript、inference、
  trace、notification 和 snapshot 原子写入 mai-store，并执行 revision CAS。
- turn factory：解析 `MaiConfig` 的动态角色路由，组合 instructions、skills、MCP 和工具。
- lifecycle：创建/回滚容器与 workspace，关闭时清理 descendants、MCP 和资源。
- observer：只处理 PL commit 已成功持久化后的产品 read model、日志、tool trace 和低频
  `MaiProductEvent`；不再把 trace 映射成另一套 UI Thread event。
- policy：把 maintainer、parent 和 task role 规则编译为 `AgentExecutionPolicy`。

mai 公共 `AgentRuntime` 仅为产品 facade，内部持有
`pl_core::AgentRuntime<MaiAgentHost>` 和非泛型 handle。mai 的 agent record 不保存 Thread actor、
active turn、pending queue、cancel flag 或 turn lock。

## 配置

`MaiConfig` 位于 mai-runtime，通过组合 `pl_core::AgentModelConfig` 形成完整 serde 文档。
mai-store 的 `ConfigDocumentStore` 只提供泛型 load/save 与原子文件替换；SQLite store 命名为
`MaiStore`。mai-protocol 继续保持轻量，只定义外部 DTO，并在 mai-runtime 单点转换。

## 可复用工具能力

Web Search 的路径选择和 MCP 的运行状态机均属于 PL，不属于产品：

- `pl-core::plan_web_search` 只读取已解析 provider/model 能力与产品配置，确定 standalone、
  hosted 或不可用路径。Studio 与 Mai 只能应用计划，不能按 provider ID 另写选择规则。
- `pl-core::McpRuntime<H>` 负责 fingerprint、并行探测、增量 reconcile、工具发现、命名冲突、
  generation、lease、health 与失败隔离。
- 新 generation 在所有 server 完成探测后原子生效；准备期间旧 generation 的活动 turn 可继续
  调用。最后一个旧 lease 释放后，PL 才关闭不再复用的 session。
- `mai-runtime::mcp::ContainerMcpRuntimeHost` 只负责 transport：Streamable HTTP 在 server 进程
  建立，stdio 通过 Docker exec 在 agent 专属 MCP sidecar 建立。sidecar 使用
  `MAI_SIDECAR_IMAGE` 并继承 agent 容器的 workspace mounts，不实现第二套 reconcile 或工具命名逻辑。
- 每个 agent 拥有一个 MCP handle 和一个独立 sidecar。agent 容器销毁时先关闭 handle 并删除
  sidecar；配置或 provider secret
  变化时，Mai 并发要求所有活动 handle reconcile；Turn Factory 每轮只获取固定 lease。

Mai 的 MCP 配置由 agent、system 与 project scope 组合。project agent 同时获得 agent/system
配置和当前项目配置。PL 内置 Zhipu Search、Reader、ZRead、Vision；四者有 Coding Plan 凭证时
默认启用，均声明为 `Read` effect。内置 identity 与 endpoint 锁定，Mai 只保存启停状态。

HTTP API 只返回公共 descriptor、脱敏 endpoint 和 agent 聚合 health。token、header value 与 env
value 都是 write-only：空值保留，显式 clear 才删除。Host 错误在进入 health、trace 或模型输出前
由 PL 统一脱敏。

## 状态协议

产品状态由资源与框架 runtime 状态组成：

```text
AgentState
├── resource: Provisioning | Ready | Deleting | Failed | Deleted
└── runtime: lifecycle + activity + activeTurn + pendingInputs + lastTurn
```

事件使用携带完整快照的 `AgentStateChanged`。任务、项目和 review 的完成判断读取 last turn
outcome，不把 agent 自身标记为 Completed。

## 统一 Thread 消息

消息、tool、Todo、context、usage、interaction、agent activity 和 turn lifecycle 统一使用
`pl-protocol::ThreadItem`、`ThreadTurnHistory` 和 `ThreadSubscriptionUpdate`。`pl-core` 按 Thread
持有独立 channel；Mai Server 的 `GET /threads/{threadId}/events` 只桥接该 subscription，
不解析或改写消息内容。

稳定状态先与 Thread transaction 一起持久化，再广播；CAS conflict 或 transaction 失败不得
产生 notification。subscription 首帧是 authoritative snapshot；channel lag、Thread revision gap
或 Item revision gap 使当前 generation 失效，Web 重新订阅。历史读取独立使用 `ThreadTurnPage`。

project、task、review、provider、settings 和资源列表使用独立 `MaiProductEventEnvelope`，通过
`GET /events/product` 推送。产品事件不得承载 Thread timeline，也不得触发整页 AgentDetail
重拉。旧 `/events` 与混合 `ServiceEventKind` 删除，不保留兼容入口。

## 持久化与恢复

mai-store schema 29 分离产品 metadata 与 framework runtime state，持久化 Thread actor、
mailbox、Turn、Item、interaction、transcript、inference、trace、notification 和 snapshot。一个
Thread commit transaction 原子写入全部运行事实，并以独立表保存 PR merged 终态。生产服务
不读取旧 schema；v27 或 v28 数据只能在停服后通过 `mai-migrate-v27` 单事务迁移，失败时整体
回滚。

启动时先 reconcile 容器和 workspace，再注册恢复的 agent actors；遗留 Running turn标记为
runtime restart 取消，资源 ready 后按 FIFO 恢复输入。

## 项目仓库与 review revision

项目 Docker volume 是项目拥有的 canonical repository，不属于 maintainer 或任何 agent：

```text
/workspace/repo.git  bare mirror，用于 fetch 和派生隔离工作区
/workspace/repo      最新默认分支的 detached working tree
```

项目创建、服务启动恢复、默认分支 push 和每次 review preparation 都在项目级
`repo_sync_lock` 下更新这个 repository。旧的 bare-only volume 会原地补建 working tree；
working tree 损坏时只重建 `/workspace/repo`，不删除 mirror、配置或数据库。同步项目 repository
不会再重置 maintainer workspace。

一次 review 先从 GitHub 解析当前 PR head，再同步默认分支和精确 PR ref。技能与
`AGENTS.override.md`、`AGENTS.md`、`Agents.md` 只从项目默认分支 working tree 提取，并保存为
run-scoped 不可变快照。随后 reviewer volume 从项目 bare mirror 创建唯一一份 clone，
`/workspace/repo` 精确检出 PR head，同时保留 `origin/<default-branch>` 作为 diff base。

reviewer 可读取、写入一次性工作区并运行进程，但没有 branch control 或 push 权限。其 GitHub
工具只允许访问项目 base repository。提交 review 前服务端再次读取 GitHub 当前 head，拒绝
stale revision，并由 scheduler 将目标重新入队；提交体的 `commit_id` 由服务端固定为准备时
验证过的 head SHA。
