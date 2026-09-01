# Project Review 生命周期

本文档记录项目 PR 审查的持久化生命周期、重试和 GitHub 提交契约。

## 领域对象

一次逻辑审查由 `ProjectReviewJob` 表示。Job 固定绑定项目、PR 和目标 head SHA，是排队、恢复、重试和最终提交的唯一事实源。一个 Job 可以产生多条 `ProjectReviewRun`；每条 Run 只表示一次 Agent turn 尝试，并通过 `job_id`、`attempt_index` 关联 Job。

Job 状态机为：

```text
Queued -> Preparing -> Running -> SubmissionPending -> Reconciling -> Succeeded
                     \-> RetryWaiting ----------------------/
                     \-> Failed | Cancelled | Superseded
```

`RetryWaiting` 是活跃状态，不是最终失败。`ProjectSummary.review_status` 只是当前活跃 Job 的只读投影，不能反向驱动 Job，也不能被 selector 状态覆盖。

## 持久化入队与幂等

独立 discovery scheduler 和 `pull_request` webhook 必须先执行确定性的 eligibility 判断，再把
合格 PR 直接写入持久化 Job 队列。`check_run`、`check_suite`、`workflow_run`
的 `completed` 事件是持久化唤醒信号：只要项目仍启用自动审查，就直接写入或
命中现有 Job，最终 eligibility 留到 Job 被 claim 后、创建 Reviewer 前执行。生产
路径不使用内存 relay 队列或内存 PR 池保存执行意图。

- 同一 `project + PR` 只保留一个非终态 Job；相同 head 的后续 completed delivery
  更新现有 Job，不重复排队。
- 新 head 到达时，旧 head 的未完成 Job 进入 `Superseded`。
- 同一 webhook delivery 对同一 PR 幂等；一个 check suite delivery 可以分别为多个关联 PR 建立 Job。
- GitHub 对 fork PR 发送的 `workflow_run` 可能没有 `pull_requests`；此时使用同项目、
  同 head 的持久化 Job 历史或 CI watch 恢复 PR，不把完成信号静默确认为
  `Ignored`。
- 手动重新审查遇到同一 PR 的活跃 Job 时直接返回该 Job，不访问 GitHub、也不创建重复 generation。
- 历史 Job 已终止时，手动重新审查可以创建新 Job。
- discovery 不为同一 head 已耗尽尝试并进入 `Failed` 的 Job 自动创建无限 generation；
  新 head、Webhook 新事件和手动重新审查不受此抑制规则影响。
- `pull_request` webhook 单 PR eligibility 读取失败时不写内存队列，由 relay
  的失败确认机制重投；当前事件仅因 CI 尚未完成而不满足 eligibility 时，按
  `project + PR` 持久化 CI watch。相同 PR 的后续 delivery 更新同一 watch，新
  head 原子替换旧 head。

`pull_request` 的 `opened`、`reopened`、`synchronize`、`ready_for_review`，以及
`check_run`、`check_suite`、`workflow_run` 的 `completed` 会唤醒对应 PR。`push`
不创建 Review Job，仍只同步默认分支与项目缓存。

server 启动后每分钟复核到期的 CI watch；独立 discovery 每 10 分钟执行一次全量兜底扫描。
CI 仍运行时 CAS 延期；变为合格时走同一个 PR 单活入队事务并删除 watch；PR 已
关闭、变为 draft 或当前 head 已审查时删除 watch。GitHub 读取失败保留 watch
等待重试。这样 completed webhook 偶发丢失或 server 重启都不会丢失待审查意图。

## Discovery 契约

discovery 是 Runtime 独立拥有的调度能力，不依附项目 worker 或 Review Job 执行生命周期。
selector 是其中确定性的 Rust 读取阶段：只读取 GitHub 并形成批量准入输入，不创建 Agent、
不调用模型、不提交 review，也不在 GitHub 网络读取期间持有 SQLite 写事务。

周期扫描使用：

```text
state=open&sort=created&direction=asc&per_page=20&page=N
```

必须完整读取所有 open PR 页面。每页按 PR number 升序评估，最多并发检查四个候选。
单候选读取失败记录为 partial error，其余候选继续；项目身份或 PR 分页读取失败则整轮失败，
不得写入任何 Job 或 watch。单 PR webhook 路径复用相同规则：

1. draft 跳过。
2. `queued`、`requested`、`waiting`、`pending`、`in_progress` 等未完成 CI 状态会阻塞；CI conclusion 不参与阻塞判断。
3. 查找当前 reviewer login 最近一次带 `submitted_at` 的 review。
4. 能读取当前 head 提交时间时，仅当该时间晚于最近 review 才允许重审。
5. 无法读取提交时间时，以 review `commit_id` 是否等于当前 head 兜底。
6. PR 作者、其他人的 review 状态和 `mergeable_state` 不参与过滤。

全部 GitHub 读取结束后，合格候选和 CI pending 候选在一个短 `BEGIN IMMEDIATE` 事务中
分别批量写入 Job 和持久化 CI watch。任一写入失败整批回滚；同 head 去重、新 head
supersede、CI watch 原子 upsert 和 PR 单活约束均复用同一 Store 不变量。只有真正新建的
Job 发布 `project_review_queued`，deduped、watched 和 suppressed 只进入 discovery 计数。

scheduler 在服务启动时立即扫描，此后以每轮开始时间为基准按固定 10 分钟周期运行；扫描超过
一个周期时跳过已经错过的 tick，从完成时重新安排下一轮，避免补跑风暴。项目并发上限为 2，
同项目绝不重叠。开启自动 Review、项目资源转为 Ready 或相关配置变化会唤醒 scheduler；
读取失败按 1 秒起步、最高 600 秒的指数退避重试。scheduler 关机时取消并排空当前扫描。
discovery 快照与 `ProjectSummary.review_status` 正交，不能覆盖活跃 Job 投影。

## Claim、租约与启动恢复

worker 只从数据库原子 claim 到期 Job。claim 写入实例 owner，租约为 60 秒，每 15 秒续租。数据库事务保证同一 Job 只有一个 owner；项目内任一仍存活的执行租约会阻止另一个实例启动新 Job。

非手动 Job 被 claim 后必须再次读取当前 head、review 历史和 CI。此时 CI 仍在运行
则在同一事务中进入 `Skipped(CiPending)` 并登记 CI watch，不创建 Reviewer；
下一条不同 completed delivery 可以建立下一代 Job，completed delivery 丢失时由
watch 复核补入。若最终检查与跳过事务之间有新 completed delivery 更新了当前 Job，
delivery 条件写入失败，worker 必须重新读取同一个 Job 并再次检查，不能丢失这个
唤醒信号。

同一项目只允许一个逻辑 Review 占用 Reviewer。尚未到期的 `RetryWaiting` 或 `Reconciling` Job 会阻塞后续排队 Job；到期后优先恢复原 Job，再处理新 Job，避免保留 Thread 的 Reviewer 与新任务竞争。

新 head 将运行中的旧 Job 标为 `Superseded` 时保留其租约。旧 worker 在下一次心跳发现失去有效状态后取消 Agent turn、清理 reviewer 并主动释放租约；若旧实例消失，新 Job 最多等待旧租约自然过期。

server 启动时：

- 已有 GitHub 回执的 Job 直接视为成功。
- 有提交意图但无回执的 Job 进入 `Reconciling`。
- 无提交副作用的过期 `Preparing`、`Running` Job 进入立即到期的 `RetryWaiting`，对应 Run 标为 `Interrupted`。
- Run 归档失败属于持久化边界失败，worker 保留 Job 的 `active_run_id` 与租约，等待过期恢复先归档 Run，再转换 Job；不得把它当成普通 Review 失败提前清空 Run 所有权。
- 对旧版本已经留下的“Job 无 `active_run_id`、Run 仍未结束”状态，仅在 Job 没有其他 active Run 且租约已失效时归档为 `Interrupted`；恢复不删除 attempt 记录，也不覆盖仍有效的执行者。
- 尚未过期的租约继续等待，支持滚动部署时的跨实例排他。

## Reviewer 与 Thread 生命周期

Reviewer 是普通 project agent，但生命周期绑定 Job，而不是单次 Run。

- 首次尝试创建 Reviewer、canonical Thread、精确 head 工作区和只读默认分支上下文。
- Reviewer system prompt 带有不可变的 Job ID、PR 和 head marker。若服务在 Agent 已持久化、但 `reviewer_agent_id` 尚未回写 Job 的窄窗口重启，只能由 marker 完全匹配的同一 Job 认领；不同 head 或不同 generation 不得复用。
- 可重试失败保留同一个 Reviewer、Thread 和会话笔记；下一次尝试启动新的 continuation turn，不重放失败 turn。
- 普通重启后，产品先恢复 Reviewer 身份；第一次查询进度或继续尝试时按 ID 惰性恢复持久化
  `ThreadActorState`，并按 Job 固定的 head SHA 重建工作区和 Review 上下文。启动过程不全量驻留
  所有 Agent。
- Thread 丢失或损坏是永久失败，不以空 Thread 静默重审。
- 只有 Job 进入 `Succeeded`、`Failed`、`Cancelled` 或 `Superseded` 后才删除 Reviewer、live Thread、上下文和工作区；终态清理也使用 marker 找回尚未写入 Job 的 Reviewer，timeline 只保留在 Run 的不可变 `ThreadTurnHistory` 归档中。

Review 上下文中的项目 Skill 从同一只读仓库快照生成。Mai 一次导出
`.claude/skills`、`.agents/skills` 和 `skills` 的仓库相对布局，再将仓库内相对
symlink 实体化为普通目录和文件；绝对、越界、悬空或循环链接立即失败。交给 PL 的
每个 Skill 根都不含 symlink，根顺序与同名 Skill 选择继续由 PL 的原生 catalog
规则负责，Mai 不建立第二套名称注册或运行时过滤逻辑。

PL v2 的 schema 31→32 升级是一次明确例外：部署窗口先正常取消并清理所有 Review Agent，完整
v1 数据库离线归档，在线库不转换旧 Thread/Timeline。升级健康后按最新 PR head 为被取消目标建立
新的 Review Job/Run，不能把旧 Review Thread 当作损坏数据恢复。

## 结构化错误与重试

PL 通过 `TurnFailure` 传递错误类别、provider code、HTTP status、用户可读消息与 `RetryDisposition`。Mai 不解析模型错误字符串来识别 `server_is_overloaded`。

PL 在单次模型请求内部仍可重试瞬态 provider 错误，但仅限尚未产生工具副作用的阶段。内部重试耗尽后，结构化失败交给 Job scheduler。

每个 Job 最多五次尝试。第一次可重试失败开启 30 分钟窗口，后续四次本地退避依次为 5 秒、30 秒、2 分钟、5 分钟，并加入确定性的正负 20% jitter。Provider `Retry-After` 更长时优先使用，但不能把新尝试安排到窗口之外。窗口只限制启动新尝试，不中断已经正常运行的尝试。

鉴权、权限、输入校验、目标不存在和 Thread 损坏立即永久失败。head 变化进入 `Superseded`。瞬态 GitHub、relay 和工作区错误可进入 Job 重试，但同样必须持久化为结构化失败。

## Watchdog

- `Preparing` 最长 5 分钟。
- `Running` 连续 10 分钟没有模型、工具或进程 revision 进展时取消 turn。
- 活跃 `exec` 使用其 `timeoutSeconds + 60 秒` 与 10 分钟中的较大值。
- GitHub 提交 sidecar 最长 2 分钟。
- `Reconciling` 最长 5 分钟，每 10 秒查询一次。
- 终态 Reviewer 清理最长 2 分钟。

租约心跳更新失败时重新读取 Job：如果当前 Job 已有有效提交回执，则只停止心跳并允许 turn 正常完成收尾；其他租约丢失或读取失败场景立即取消本地尝试，避免无法确认 owner 时继续产生副作用。取消 Agent turn 会同时触发 PL 对受管 exec 进程的清理。

## GitHub 最终提交幂等

Reviewer 调查期间不得提交单条评论。最终 review POST 前，Mai 必须先持久化 `SubmissionIntent`，包括 Job ID、head SHA、event、正文 hash、评论数量和创建时间。服务端自动在 review 正文追加：

```html
<!-- mai-review-job:{job_id} -->
```

POST 成功后持久化 GitHub review ID、event、commit SHA、URL 和提交时间。若请求结果不确定、响应解析失败、回执落盘失败或服务重启，Job 只能进入 `Reconciling`：按隐藏标记和 head SHA 查询 GitHub，找到后补写回执，不得再次发送完整 POST。

Continuation 只允许把同时包含当前 Job 隐藏标记、且 commit SHA 等于固定 head 的 review 视为本 Job 已提交。其他 Job、其他 head 或无标记的历史 review 仅作为审查上下文，不能据此返回 `review_submitted`。

GitHub 明确返回 `Line could not be resolved` 时，允许同一 SubmissionIntent 去掉 `comments` 后执行一次 body-only 恢复；正文、event 和 head 必须保持一致。除此之外，未解决 intent 不允许第二次 POST。

GitHub 回执是成功的硬条件：已有回执时，即使最终模型 JSON 或 turn 收尾失败，Job 仍成功；模型声称 `review_submitted` 但没有持久化回执时，Job 以 `missing_submission_receipt` 永久失败。

## API 与展示

列表和详情 API 以 Job 为主对象，详情按 `attempt_index` 展示各次 Run。Web 必须把
`RetryWaiting`、`Reconciling` 显示为活跃阶段，并展示结构化错误、下次重试时间、
SubmissionIntent 和 GitHub receipt。可用的 v2 Run detail 返回业务结果和不可变
`ThreadTurnHistory`，不得重新订阅已经删除的 Reviewer Thread。

schema 32 继续保留旧 Job/Run、intent、receipt、状态、用量和时间事实，但旧 Timeline 只存在于
`framework-archives/pl-v2-<timestamp>/`。对应 Run 返回 `history_status = pl_v2_archived`、归档 ID
和时间；Web 显示明确的离线归档说明，不发起旧 Thread 请求，不使用加载失败或损坏文案。旧终态
不自动重放；迁移前受控取消的目标在新服务健康后按最新 head 重新入队。

`GET /projects/{id}/review-discovery` 返回 Runtime 内存快照，状态为 `disabled`、`idle`、
`scanning`、`partial` 或 `backoff`，并包含上次开始/完成、下次计划时间、scanned、eligible、
queued、deduped、watched、suppressed、errors 和最近错误。快照不写产品数据库；重启后先进入
`scanning` 并立即生成新结果。

每次快照变化发布 `project_review_discovery_updated` SSE。Web Review 页面用该事件精确失效
discovery query，同时保留 60 秒低频刷新作为断线兜底；状态卡与 Job 状态、Run 状态分开展示。
