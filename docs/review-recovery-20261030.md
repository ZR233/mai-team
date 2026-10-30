# 自动 PR 审查停滞：2026-10-30 故障记录

## 已确认的原因

目标 `runner@10.3.10.194` 的 `mai-server` 进程与 `/health` 正常，自动审查 worker 被恢复检查阻塞。

1. 服务账户仅被授予 `/mnt/docker_nvme` 的穿越权限。该权限足以访问它拥有的产品目录，符合最小权限设计。
2. 命令已正常执行并保留 stdout/stderr，但 `MaiResourceStore` 发布对象后对全部祖先目录逐级 `File::open + fsync`，最终打开挂载根失败。canonical effect 96 保存了真实错误 `Permission denied (os error 13)`；PL 因输出存储失败进入 `blobFailed`，停止继续执行。
3. watchdog 中断后，尚未被模型消费的旧输入仍为 Pending。后续 Run 提交了新输入，而 PL 按队列顺序选择旧输入；新 Run 的受理输入没有生成对应 Turn。原取消函数只匹配活动 Turn，对这个排队输入无操作。
4. PR #2481 head 更新后旧 Job 已 `Superseded`，Run `9334621e-d2bd-4159-851b-8263f0a13c8d` 仍为未完成。恢复归档要求其 Turn 历史，但该输入确实从未启动 Turn；同一记录使两个项目的 worker 都无法走到领取任务。诊断时 tgoskits 有 3 个排队 Job。

实时二进制 SHA256 为 `5e2626bdfec80fe3ac58dbf6c0514e3dea10d37c3952a6cb1bc6f773a0e60c95`，对应共享源码发布代码 `2f9523b97b76c42f71cab975fd1a18f3e0bad7a6`；会话仍是 PL b4213a1/schema 11。不能因主线已经升级 PL 而把另一轮会话迁移混入故障恢复。

## 修复边界

- 持久目录逐级创建时，只同步新目录本身及包含新目录项的父目录；对象原子发布只同步直属资源目录。既有祖先没有变化，不打开、不要求列目录权限。I/O 错误仍向上传递，输出存储错误保留具体路径，不扩大挂载点 ACL。
- 取消接受 PL 的两种真实身份：活动 Turn 通过精确 `interrupt_turn`；尚未消费的输入通过 `discard_input`。不会取消无关活动 Turn，也不会把旧 Pending 输入留给下一次 continuation。
- Review 快照使用 PL typed effect 查询，从最新记录读到目标输入的受理记录，只投影该输入的真实执行历史。用量读取同一 PL 耐久 checkpoint，不依赖 Thread 是否驻留。
- 新增 `history_status=not_started`：仅在已查到受理事实且未发现该输入启动或消费事实时产生。未启动的非成功 Run 可以记录中断并释放自身 Job 所有权；有 GitHub 成功回执的 Run 仍必须归档真实 Turn 历史。丢失、损坏、查询失败、已启动但无终态等情况继续显式报错。
- Web 对“已受理但 Turn 未启动”显示明确说明；不伪造空 Turn，不把历史缺失改成成功，不删除或重写既有 effect、checkpoint、归档、提交意图或 GitHub 回执。

## 验证与恢复

证据根：执行机 `/home/zhourui/.codex/artifacts/mai-server-stall-20261030`。

- `ancestor-access-proof.json`：用生产服务账户执行只读目录打开与同步，资源根成功，挂载根明确 errno 13。
- `permissions-red.log` / `permissions-green.log`：真实仅穿越祖先的同一回归先失败后通过，13 项资源存储测试通过。
- `not-started-red.log` / `cancel-input-red.log` / `runtime-green.log`：未启动归档和精确排队取消先记录失败；修复后 runtime 355 通过、1 项实机恢复竞态测试忽略。native PL 存储测试还核对未知输入和已启动未结束输入不会误判为未启动；成功回执缺历史的既有保护测试继续通过。
- 前端 typecheck、lint、129 项 Vitest 和 build 通过。

生产恢复使用部署提交的同范围补丁，保持原 PL rev、schema 和数据目录。停服后备份二进制、配置、产品库、会话目录以及实际拥有的执行环境清单；正常启动通过自身恢复与清理接口释放旧 Job，随后观察新 head 的任务领取和模型/工具进展。不能手工把 Run 改成成功、清空 `active_run_id` 或重发旧提交。

## 本轮交付与生产结果

- 主线修复代码提交 `d4ae6e8cd6020a18fcdb193b2c17d9a7d68f14bd`，PR [#47](https://github.com/ZR233/mai-team/pull/47)。主线锁定依赖全工作区测试 805 通过、2 项外部环境用例未执行；Clippy 通过，保留既有 10 项 `async_trait` 的 `double_must_use` 警告；格式检查通过。前端 129 项 Vitest 与三种视口 30 项 Playwright 通过。Playwright 使用 HTTP 夹具，Vite 的未夹具化 `/agents/agent-1` 代理请求本地 8080 被拒绝，不将这类日志当作生产 HTTP 验收。
- 生产回移提交 `a2859e3d8f9692f3b7c138c5f30a193e911c31a2`，分支 `codex/review-recovery-production`。只包含同一故障补丁；原 PL b4213a1、会话 schema 11、产品库 schema 38 保持，未执行额外迁移。该提交 runtime 352 项通过、1 项未执行，锁定依赖 release 构建通过。
- 生产二进制 SHA256：`ca6087e742a1ae47134ddad686a99662a4cb708dc94fa9fa1d67745c043fb195`。已核对运行进程的 `/proc/<pid>/exe`，不是仅校验磁盘上的待启动文件。
- 香港时间 2026-10-30 10:32:20 停服，10:35:19 健康就绪。完整备份在目标机 `/mnt/docker_nvme/mai-team/incidents/review-recovery-20261030T023220Z`，根目录权限 0700，包含约 46 GB 会话在内的整个数据目录、原二进制、配置、旧 Reviewer 容器可写层及其 workspace/review view 两个卷的归档；`backup-manifest.json` / `deployed.json` 记录身份、时间和摘要。
- 启动自身恢复记录 `terminal_runs=1`，旧 Job 仍为 `superseded`，旧 Run 变为 `interrupted/not_started`，原输入身份保留，`active_run_id` 和过期 lease 由原生事务释放。随后 singleton 正常清理旧 Reviewer。
- 335 个原 Run 没有缺失，只有故障 Run 的终态、错误、用量和历史状态按恢复更新，其他 334 个原 Run 整行字节事实未变；原会话目录无缺失。没有手工修改业务表或 session effect。
- worker 已领取 PR #2707（新 Job `f24c36a2-4cff-4ca6-a186-def207873f86`），模型调用、工具和耐久 effect 持续增加。首次实机采样已有 7 条 `exec` 成功执行并产生归档资源，存储 fault generation 为 0；接口约 3 毫秒返回，恢复后的日志无 EACCES 或旧恢复错误循环。此结论是调度和执行恢复，PR 审查本身仍需以最终 GitHub 回执判定完成。

## 回退边界

不要仅覆盖回旧二进制：新的 `not_started` 业务状态属于本次完整内部接口变更。若确需回退，先停止服务并另存当前数据和执行环境，再从上述成套备份恢复原二进制、数据目录及需要的旧 Reviewer 镜像/卷；归档挂载身份与路径在清单中，不能把镜像 commit 当成卷数据。

回退的数据时间点为香港时间 2026-10-30 10:32:20。该时间点后的新审查写入和回执必须先单独保全、明确取舍；不能自动覆盖新增业务写入。回到旧代码也会重新带回已定位的缺陷，因此本轮未执行回退、未通过自动重放旧 Job 恢复队列。
