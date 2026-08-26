# mai 产品宿主与 PL v2 框架边界

## 目标

mai 是基于 `pl-core` 的产品宿主。PL 唯一拥有 `Thread → Turn → Item/Interaction`、Agent
执行状态、mailbox、模型循环、工具计划、Skill 目录和动态 MCP 运行时；mai 只拥有项目、任务、
容器、工作区、GitHub、Review Job/Run、产品策略和产品资源生命周期。

两者是同一产品内的源码边界，不是需要版本协商的公开网络协议。边界变化时同时修改两端，
不保留旧结构别名、兼容读取器、双路径或运行时适配层。

## 状态与身份

产品 `AgentSummary` 只组合两块正交事实：

```text
AgentSummary
├── resource: AgentResourceSnapshot
│   └── Provisioning | Ready | Deleting | Failed | Deleted
└── runtime: Option<pl_protocol::AgentSnapshot>
    └── AgentState { kind, data }
        ├── Idle | Queued | Running
        ├── WaitingTool | WaitingInteraction | Cancelling
        └── Closing | Closed | Faulted
```

容器、workspace 或 sidecar 失败只改变 `resource`；Turn 执行、排队、取消或故障只改变 PL
`AgentState`。mai 不再保存 lifecycle、activity、active/last turn 或 pending queue 的重复投影。
API 和 Web 必须穷尽匹配 PL tagged state，不能从旧扁平字段推断。

长期产品 Agent 身份先从产品表恢复，PL actor 按需驻留。首次读取、发送、取消、等待、订阅、
Review 进度查询或删除前统一执行：

```text
product Agent id
  -> ThreadRepository::restore_thread
  -> AgentRuntimeHandle::restore_agent
  -> canonical operation
```

`restore_runtime` 只返回明确的启动钉住集合；mai 当前没有启动钉住 Agent，因此返回空集合。
并发首次访问由 PL coordinator 串行并幂等注册。schema 32 升级后保留的长期产品 Agent 没有
v2 Thread 文档，首次访问会创建全新的 v2 初始状态；这不是旧数据转换，也不会读取已归档历史。

## Thread 持久化

`MaiAgentRepository` 是 PL `ThreadRepository` 的唯一实现。一次 `ThreadCommit` 将 actor document、
mailbox、Turn、Item、Interaction、notification、trace、submission 和计费事实转换为一个
SQLite CAS transaction。数据库提交成功前不得广播 authoritative notification。

写入由单一所有者的 FIFO writer 接受：

- `commit` 只把完整 revision 加入有界队列；
- `await_durable(thread, revision)` 精确等待指定 revision，不用“队列为空”代替；
- revision conflict 或 I/O 失败会固定为 writer 故障并传播给所有 barrier；
- 关机先停止接收，再排空全部已接受提交；
- 从 durable Thread 恢复时以其 revision 初始化 barrier 水位。

任何不可逆副作用前都必须越过对应 Thread revision 的 durability barrier。当前 GitHub 非 GET
请求在产品 Typed Tool 内执行该约束，不能在未持久化 intent 或 Turn 事实时调用外部写接口。

## 工具所有权

PL `ToolManager` 为每个 Agent 创建隔离的 `AgentToolSet`。工具按普通 `ToolGroupId` 安装，模型
调用前刷新动态组并冻结唯一 `ToolPlan`；一次模型调用的重试和工具执行始终复用同一计划。
组刷新失败保持上一代，不会发布半成品。

mai 只负责构造产品 Typed Tool、声明 `ToolEffect` 并按角色安装允许的工具组。权限由“没有安装
该组”表达，不存在运行时可见性过滤器。MCP、Skills、Web Search、LSP、PL builtin 和 mai
product tools 都遵循相同工具组生命周期；exclusive Web Search 通过卸载不允许的组获得唯一
计划，而不是在 schema 列表上二次过滤。

## Skill 所有权

PL 公开 `SkillRegistry`、目录 provider、`FrozenSkillCatalog`、`skills_list` 和 `skill_view`：

- PL 负责扫描 `SKILL.md`、frontmatter 校验、名称去重、generation、冻结、正文加载、support
  resource 路径安全和 Turn Skill 指令；
- mai 只声明 project/repository/user/system 目录及顺序，保存产品禁用名称，并把同一 frozen
  catalog 的 System/Project 目录投影进 Agent 容器；
- project Skill 的 sidecar 探测、精确 Review base SHA 快照、项目锁和缓存仍由 mai 持有；
- 每个 Turn 只构建一个 catalog，显式 `skillMentions` 和文本调用都通过 PL 加载；模型看到的
  Skill 工具也来自这个 catalog；
- system Skill 是否启用服从 PL `SkillsConfig.system.enabled`，mai 不另写解析器或回退规则。

配置 API 只接受 `{ "disabled": [name...] }`。schema 31 的旧 `config[].enabled` 仅在离线
31→32 迁移中读取一次；schema 32 服务不会解析旧形状。

## MCP 与动态工具

`pl-core::McpRuntime` 负责 fingerprint、并行探测、增量 reconcile、工具发现、命名冲突、
generation、health 与失败隔离。Mai 的 `McpConnector` 只把 stdio command 改写为 Agent sidecar
中的 `docker exec -i`；Streamable HTTP 保持宿主直连。配置或 secret 变化要求各 Agent handle
重新 reconcile，但不能在 mai 建第二套 MCP 状态机。

每次模型调用前，Turn Engine 刷新 MCP 和其他动态组并原子替换 ToolPlan。Agent 容器销毁时先
关闭 MCP handle，再删除 sidecar。HTTP 只返回脱敏 descriptor 和聚合 health；token、header
value 与 env value 始终 write-only。

## 指令与模型循环

PL 选择并组装模型或 builtin base prompt。Mai Turn Factory 只提供角色、产品、workspace、
Review 和动态上下文 overlay。显式调用 Skill 时通过 `TurnRequest::with_skill_invocation_instruction`
加入当前 Turn；该指令不写入 transcript，也不会污染后续 Turn。

Mai 不修改 PL session 来伪造 lifecycle，不从 trace 反向拼装 Agent 状态。稳定执行事实只通过
PL command/state transition 产生。

## Thread 与产品事件

`GET /threads/{threadId}/events` 直接桥接 PL `ThreadSubscriptionUpdate`。首帧是 authoritative
snapshot；channel lag、Thread revision gap、Item revision gap 或 ownership 失败使当前 generation
失效，Web 重新订阅。历史分页只读取 `ThreadTurnPage`。

项目、任务、Review、provider、settings 和产品资源使用独立 `/events/product`。产品事件不得
携带 Thread timeline，也不得触发旧 Agent runtime 字段的兼容解析。

## schema 32 归档边界

生产服务只接受 schema 32。`mai-migrate` 只实现 31→32：

1. 使用 SQLite backup API 在 `framework-archives/pl-v2-<timestamp>/` 创建不可自动清理的完整
   旧库；
2. 生成包含源/目标提交、SHA256 和关键表行数的清单，备份或校验失败立即中止；
3. 在线库保留项目、PR、Review Job/Run、intent、receipt、状态、用量和时间等产品事实；
4. 删除旧 PL runtime/history JSON 与 Review Timeline，写入 `pl_v2_archived` 标识；
5. 保留长期产品 Agent/Thread 身份，但不转换 v1 执行态；Review Agent 在迁移前正常取消并清理。

Web 对归档 Run 明确展示“PL v2 升级离线归档”，不能把缺少 Timeline 表现为加载失败或数据损坏。

## 项目仓库与 Review revision

项目 Docker volume 是项目拥有的 canonical repository：bare mirror 用于 fetch 和派生隔离工作区，
默认分支 working tree 用于项目上下文。项目创建、启动恢复、默认分支 push 和 Review preparation
都在项目级同步锁下更新，不重置 maintainer workspace。

一次 Review 先固定 GitHub 当前 PR head，再建立精确 head 的 reviewer workspace 和默认分支只读
上下文。Reviewer 可以读写一次性工作区并运行进程，但没有 branch control 或 push 权限。
提交前服务端再次读取当前 head，拒绝 stale revision；GitHub `commit_id` 必须等于已验证 head，
receipt、event 和隐藏的 Review Job marker 必须一致。
