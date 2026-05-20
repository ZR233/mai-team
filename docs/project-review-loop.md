# Project Review Loop

本文档记录自动项目 PR 审查的运行时契约。

## 职责划分

每个已就绪且启用自动审查的项目拥有一个可取消的审查任务上下文。该上下文会启动：

- PR 池 worker：只消费已经确认合格的 PR，并一次审查一个 PR。
- provider selector：按 Git provider 的节奏扫描 GitHub open PR。
- relay select-pr loop：消费 relay PR 队列，复用 selector 条件过滤单个 PR。

reviewer 是普通 project agent：由项目 agent 机制创建，使用 FreeRootSandbox、agent workspace volume 和 gh sidecar 执行一次 PR 审查。它不拥有独立于 project agent 生命周期的特殊运行时通道。

禁用、取消和删除项目审查时，运行时会取消该上下文、终止子任务、清空 relay PR 队列和现有 PR 池，并清理活跃 reviewer agent。

## 队列契约

项目审查有两级队列：

- relay PR 队列：接收 relay webhook 归一化出的 PR 信号，按项目内 `pr id` 去重。
- PR 池：只接收 selector 或 relay select-pr loop 确认为合格的 PR。

relay handler 只处理 PR 相关事件：

- `pull_request` 的 `opened`、`reopened`、`synchronize`、`ready_for_review`。
- `check_run` 和 `check_suite` 的 `completed`。

这些事件能解析出 PR id 时进入 relay PR 队列；重复 PR 保留最新的 `head_sha`、`delivery_id`、`reason` 和更新时间。relay handler 在成功入队或去重后立即 ack `Processed`，不会等待 GitHub API 读取或 selector 过滤。无法解析 PR、非目标事件、未匹配项目或自动审查关闭时 ack `Ignored`。

`push` 不进入 relay PR 队列，仍按现有逻辑同步项目默认分支和 project cache volume。

## Selector 契约

selector 是确定性的 Rust 代码。它不创建 agent、不提交 GitHub review、不调用模型 API。selector 只读取 GitHub、评估 PR 是否满足审查条件，并把合格 PR 放入 PR 池。

全量 selector 扫描 open PR 时使用：

```text
state=open&sort=created&direction=asc&per_page=20&page=N
```

全量 selector 会持续扫描到 GitHub 返回空页或短页。每页先按 PR number 升序排列，再以最多 4 个候选 PR 并发评估；合格 PR 在评估完成后立即入 PR 池，入池信号使用 `reason = "selector"` 且没有 `delivery_id`。由于页内并发，入池完成顺序不作为排序契约；PR 池自身按最小 PR number 消费。

单 PR selector 由 relay select-pr loop 调用。`pr = 0` 时直接判定不合格；其他 PR 会读取 GitHub PR 详情，relay 信号中的 `head_sha` 只在 PR 详情缺少 `head.sha` 时作为 fallback。单 PR selector 复用同一套 eligibility 规则；合格后由 relay select-pr loop 用 relay 信号的 `delivery_id` 和 `reason` 放入 PR 池。

### Select PR 完整规则

候选数据由 `eligibility.rs` 从 GitHub 读取：

1. 先读取 PR 详情 `/repos/{owner}/{repo}/pulls/{pr}`。如果详情显示 `draft = true`，不再读取 review、commit 或 CI 信息，直接交给选择规则跳过。
2. 非 draft PR 会读取最近最多 100 条 PR review：`/repos/{owner}/{repo}/pulls/{pr}/reviews?per_page=100`。
3. 如果有 head sha，会读取该 commit 的 author/committer 时间，作为当前 head 的提交时间。这个请求失败时不阻断 selector，后续改用 review `commit_id` 与 head sha 的匹配关系兜底。
4. 如果有 head sha，会读取 check runs 和 legacy combined status contexts。读取失败或响应无法解析时按没有对应信号处理，不把未知 CI 当成 pending。

选择规则位于 `selection.rs`，按 PR number 升序对候选排序后逐个判断：

1. draft PR 跳过。
2. CI 中仍有 pending 状态时跳过。pending 状态只看 check run 的 `status` 和 legacy status context 的 `state`，大小写和首尾空白不敏感；以下值会阻塞：`queued`、`requested`、`waiting`、`pending`、`in_progress`。
3. CI conclusion 不参与阻塞判断。`failure`、`success`、`skipped` 等已完成结果都不会阻止入池，失败的 CI 由 reviewer 在审查中判断影响。
4. legacy combined status 的顶层 `state` 只持久到候选对象中用于观察；如果没有具体 status contexts，即使顶层 `state = "pending"` 也不会阻塞入池。
5. 查找当前 reviewer login 提交过且带 `submitted_at` 的最新 PR review。review 的 `state` 不参与去重判断，`APPROVED`、`CHANGES_REQUESTED`、`COMMENTED` 等都表示 reviewer 已在某个时间点审过。
6. 如果当前 reviewer 没有提交过 review，则在 draft 和 pending CI 规则通过后可入池。
7. 如果能读取到当前 head 的提交时间，则只用时间判断是否需要重审：`latest_commit_at <= latest_reviewer_review.submitted_at` 时跳过；只有 `latest_commit_at > latest_reviewer_review.submitted_at` 才重新入池。
8. 如果读取不到当前 head 的提交时间，则用 review `commit_id` 兜底：最新 reviewer review 的 `commit_id` 等于当前 head sha 时跳过；否则允许入池。
9. 后续其他人的 review、comment 或 `CHANGES_REQUESTED` 不会让同一个 head 重新入池；只有当前 reviewer review 之后出现新 commit 才会重新入池。
10. PR 作者不参与过滤。reviewer 自己创建的 PR 与其他 PR 使用同一套规则。
11. 当前 selector 不读取也不按 `mergeable_state` 过滤。合并冲突、分支落后或受保护分支状态不是入池条件，属于 reviewer 审查阶段需要报告的问题。

## Provider 节奏

GitHub token 项目和 GitHub App relay 项目都会运行周期性 selector。server 启动、启用审查或入队信号启动 worker 后，selector loop 会开始运行；Git provider 为空时不启动 provider selector。

selector 失败时使用指数退避重试，初始 1 秒，之后翻倍，最高 600 秒。一次 selector 成功后等待 30 分钟再进行下一次扫描；成功但没有合格 PR 时状态显示 `Waiting` 并设置下一次扫描时间，成功且有 PR 入池时状态回到 `Idle`。

selector 状态更新只是尽力而为的 UI 信号，不能覆盖活跃审查。当项目正在 syncing 或 running reviewer 时，selector 状态更新会被跳过，Web UI 继续显示活跃审查。

## Relay Select-PR Loop

relay select-pr loop 与 PR 池 worker 独立运行：

1. 从 relay PR 队列 claim 最小 PR number。
2. relay 队列为空时等待 `relay_review_notify` 或取消信号。
3. 对 claimed PR 运行单 PR selector。
4. 合格时放入 PR 池，并发布现有 `ProjectReviewQueued` 事件。
5. 不合格时丢弃该 relay 信号。
6. GitHub API、鉴权、网络失败或 PR 池入队失败时记录 warn，将 relay 信号放回 relay PR 队列，并按 1 秒起步、最高 600 秒的指数退避重试。
7. 返回 relay PR 队列继续处理。

relay select-pr loop 不直接审查 PR，也不阻塞 PR 池 worker。它只是把异步 relay 事件转换成经过 selector 过滤的 PR 池信号。

## 工作区与 Volume 契约

project cache volume 保存项目级仓库缓存和默认分支同步结果。它属于项目运行时的共享缓存，不是 reviewer 的工作目录，也不承载单次 PR 审查中的文件修改。

reviewer agent workspace volume 是单个 reviewer agent 的隔离工作区。审查开始时，reviewer 通过 helper 在自己的 workspace volume 中准备 `/workspace/repo` 克隆和目标 PR checkout；本地验证、diff 检查和审查脚本都只在这个 agent workspace volume 内执行。

gh sidecar 提供 GitHub CLI 访问能力。reviewer 通过 helper 和 gh sidecar 读取 PR 元数据、提交 inline review comments 和最终 review；不依赖旧 MCP 响应形状作为运行时契约。

## Review Run 结果契约

reviewer agent 的最终回复必须是单个 JSON 对象，运行时只以这个对象作为 Recent Runs 的结果来源，不从 summary 文本或 GitHub tool trace 反推 review 类型。

`outcome = "review_submitted"` 时，必须同时提供 `review_event`：

- `approve`：已提交 APPROVE review，Recent Runs 显示 `Approved`。
- `request_changes`：已提交 REQUEST_CHANGES review，Recent Runs 显示 `Changes Requested`。
- `comment`：已提交 COMMENT review，Recent Runs 显示 `Commented`。

`outcome = "failed"` 时，`review_event` 必须为 `null`。旧记录没有 `review_event` 时，UI 继续显示 `Review Submitted` 等 outcome fallback，不做历史文本推断。

最终 JSON 形状固定为：

```text
{"outcome":"review_submitted|failed","review_event":"approve|request_changes|comment"|null,"pr":123|null,"summary":"short result","error":null|"failure reason"}
```

## PR 池 Worker 契约

PR 池 worker 启动时先确保 project cache volume 可用，然后循环：

1. 从 PR 池 claim 最小 PR number。
2. PR 池为空时等待 `review_notify` 或取消信号。
3. 对 claimed PR 创建普通 reviewer project agent，并在该 agent 的 workspace volume 中运行一次 review cycle。
4. 仅在可重试审查错误时把 claimed PR 放回 PR 池。
5. 返回 PR 池继续处理。

PR 池 worker 不触发 selector 扫描，也不读取 relay PR 队列。selector 调度、relay 事件过滤和实际审查执行保持解耦，避免长时间 GitHub 读取让 Web UI 看起来像 reviewer 卡住。
