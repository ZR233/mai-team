# Project Review Loop

本文档记录自动项目 PR 审查的运行时契约。

## 职责划分

每个已就绪且启用自动审查的项目拥有一个可取消的审查任务上下文。该上下文会启动：

- PR 池 worker：只消费已经确认合格的 PR，并一次审查一个 PR。
- provider selector：按 Git provider 的节奏扫描 GitHub open PR。
- relay select-pr loop：消费 relay PR 队列，复用 selector 条件过滤单个 PR。

禁用、取消和删除项目审查时，运行时会取消该上下文、终止子任务、清空 relay PR 队列和现有 PR 池，并清理活跃 reviewer agent。

## 队列契约

项目审查有两级队列：

- relay PR 队列：接收 relay webhook 归一化出的 PR 信号，按项目内 `pr id` 去重。
- PR 池：只接收 selector 或 relay select-pr loop 确认为合格的 PR。

relay handler 只处理 PR 相关事件：

- `pull_request` 的 `opened`、`reopened`、`synchronize`、`ready_for_review`。
- `check_run` 和 `check_suite` 的 `completed`。

这些事件能解析出 PR id 时进入 relay PR 队列；重复 PR 保留最新的 `head_sha`、`delivery_id`、`reason` 和更新时间。relay handler 在成功入队或去重后立即 ack `Processed`，不会等待 GitHub API 读取或 selector 过滤。无法解析 PR、非目标事件、未匹配项目或自动审查关闭时 ack `Ignored`。

`push` 不进入 relay PR 队列，仍按现有逻辑同步项目默认分支和审查工作区。

## Selector 契约

selector 是确定性的 Rust 代码。它不创建 agent、不提交 GitHub review、不调用模型 API。selector 只读取 GitHub、评估 PR 是否满足审查条件，并把合格 PR 放入 PR 池。

全量 selector 扫描 open PR 时使用：

```text
state=open&sort=created&direction=asc&per_page=20&page=N
```

每页按 PR number 升序处理。全量 selector 会持续扫描到 GitHub 返回空页或短页，并且只在完整扫描成功后批量入 PR 池，避免失败扫描留下半批结果。

单 PR selector 由 relay select-pr loop 调用。它只读取 relay 队列给出的目标 PR，复用同一套 eligibility 规则；合格后用 relay 信号的 `delivery_id` 和 `reason` 放入 PR 池。

eligibility 规则位于 `selection.rs`：跳过 draft，跳过仍在运行或排队中的 CI，并在当前 reviewer 已审查当前 head 时抑制重复审查。reviewer 自己创建的 PR 与其他 PR 使用同一套规则。

## Provider 节奏

GitHub App relay 项目会运行一次启动 selector。它在 server 启动或启用审查时启动；如果扫描出错，每 10 秒重试，直到一次扫描完成。扫描成功后退出，即使没有合格 PR。

GitHub token 项目会运行周期性 selector。失败扫描每 10 秒重试；成功扫描后等待 30 分钟再进行下一次扫描。

selector 状态更新只是尽力而为的 UI 信号，不能覆盖活跃审查。当项目正在 syncing 或 running reviewer 时，selector 状态更新会被跳过，Web UI 继续显示活跃审查。

## Relay Select-PR Loop

relay select-pr loop 与 PR 池 worker 独立运行：

1. 从 relay PR 队列 claim 最小 PR number。
2. relay 队列为空时等待 `relay_review_notify` 或取消信号。
3. 对 claimed PR 运行单 PR selector。
4. 合格时放入 PR 池，并发布现有 `ProjectReviewQueued` 事件。
5. 不合格时丢弃该 relay 信号。
6. GitHub API、鉴权或网络失败时记录 warn 并丢弃该 relay 信号，等待后续 relay 事件或 provider selector 补偿。
7. 返回 relay PR 队列继续处理。

relay select-pr loop 不直接审查 PR，也不阻塞 PR 池 worker。它只是把异步 relay 事件转换成经过 selector 过滤的 PR 池信号。

## PR 池 Worker 契约

PR 池 worker 启动时先确保 review workspace 可用，然后循环：

1. 从 PR 池 claim 最小 PR number。
2. PR 池为空时等待 `review_notify` 或取消信号。
3. 对 claimed PR 运行一次 review cycle。
4. 仅在可重试审查错误时把 claimed PR 放回 PR 池。
5. 返回 PR 池继续处理。

PR 池 worker 不触发 selector 扫描，也不读取 relay PR 队列。selector 调度、relay 事件过滤和实际审查执行保持解耦，避免长时间 GitHub 读取让 Web UI 看起来像 reviewer 卡住。
