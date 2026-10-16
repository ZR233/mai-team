# PL 主线适配记录

## 版本与职责

2026-10-16 将共享参考源码分支的 PL 开发提交 `b4213a1f33ac6a8c1e2b1c403a9b4fe9ead32965` 切到 Anywork 合并 PR #86 后的主线 `8fd235ac748901ff0709c2fa65bb2475e8edf48b`（PR #86 合并提交为 `de4ad6095ace79eed68d5fc32f106f7f39971034`，随后主线仅更新产品展示文档）。五个直接依赖与十个锁定的 PL 包统一使用 `https://github.com/ZR233/anywork.git` 和同一 rev，没有升级其他注册表依赖。

Mai 继续使用 PL canonical Thread、准入 gate、任务控制工具和 cold store。任务索引、任务投递和会话 schema 转换由 PL 拥有。Mai 没有接入 PL chat/Studio 的产品回执接口，也没有使用 PL remote producer；不添加这些接口的适配器、兼容别名或旧载荷解析分支。

## 命令捕获

Mai 的 Docker 进程明确声明 `CommandCaptureOwnership::Manager`。PL 从进程事件中准入输出并持有额度；Mai 只完成固定租约内的文件写入。`CommandExit.capture` 为 `None`，不为 manager 模式伪造 producer 收据。

捕获成本直接从原始字节计算：合并文件分帧、必要的末尾换行及可选 stdout/stderr 副本逐项计费。这个同步函数不做文件 I/O、不加异步锁、不分配与输出等长的副本。命令头部在创建捕获文件之前检查固定的捕获上限，超过时命令不启动。32 KiB 模型正文上限与 64 MiB 宿主捕获总额继续由 PL 的同一命令管理器执行。

文件生命周期抽到私有 `turn/command_capture.rs`，保留原有合并文件修复语义：截断回确认偏移后重放已准入片段，重复修复不重复追加。此契约只修复 canonical 合并文件；不能据此声称故障下的分流文件也完整重建。发布到 workspace 的副本属于既有产物发布路径，不是上述宿主捕获文件总额的新增副本。

## 会话离线迁移

普通打开保留 `SessionStoreError` 类型；schema 11 不会被新版本打开时隐式转换、删除或重建。本次显式迁移只转换 schema 11→12；首次建库中断留下的 schema 0 且无业务表的空库只报告、保持原样，由 PL 正常打开时初始化。有业务表的 schema 0 和其他更旧版本明确拒绝，不提供空转换兜底。

`mai-migrate-pl-sessions` 与 `mai-server` 从同一提交构建，两个发布 workflow 将 server、该工具、独立产品库迁移工具 `mai-migrate-references` 和内部 `SHA256SUMS` 一起打包。安装升级前从发布包提取工具和 `SHA256SUMS` 并核对；通用更新脚本不能替代这次离线步骤。

1. 停止服务并确认旧进程退出。完整备份二进制、配置、产品库、整个会话目录以及恢复需要的 Docker 卷和镜像。记录停服数据时间点。
2. 使用服务的实际运行用户执行与待安装 server 同版本的工具；不能以 root 迁移后直接交给普通用户服务，锁文件和 SQLite 辅助文件也需要正确的所有权。以下为安装脚本默认的 `mai-server` 用户，其他部署按实际 systemd User 替换：

   ```bash
   sudo install -d -m 0700 -o mai-server -g mai-server /var/backups/mai-server
   sudo -u mai-server ./mai-migrate-pl-sessions check --data-path /var/lib/mai-server
   sudo -u mai-server ./mai-migrate-pl-sessions migrate --data-path /var/lib/mai-server --backup-directory /var/backups/mai-server/pl-sessions-20261016
   ```

3. 检查备份目录中的 `report.json`，已初始化会话的 `after.schema` 应为 12、`error` 为空；未初始化空库仍为 0，四张原始表的行数与摘要前后一致。随后安装并启动同版本 server。

服务与工具取得同一个持久 `.mai-server.lock` 的独占文件锁，释放后不删除锁文件。目录所有权契约由 server 与维护工具共同遵守，直接嵌入 Runtime 的其他宿主也必须遵守停服约束。服务在打开产品库前取得锁，显式相对数据路径也先统一解析为绝对路径。旧版本服务没有全目录锁，因此操作员仍须确认旧服务已停止；PL 原生迁移另有每库独占 lease，不能把它当作整个旧服务已停的证明。

工具只发现真实 `sessions/<规范 Agent UUID>/history.sqlite`，拒绝 symlink 和其他 Agent 身份的数据。检查以只读 SQLite 连接核对完整性和版本，按行流式摘要原始 `session_entries`、`session_entry_history`、`session_history_heads`、`thread_checkpoints`，不会把完整历史克隆到内存。产品库 schema 38 不在此工具职责内；从产品库 schema 37 升级时还需在同一个停服窗口执行独立的 `mai-migrate-references`。检查遇到非法条目或不支持版本会非零退出并指明原因，不把部分检查结果当作整批通过。

迁移前使用 SQLite backup API 备份已提交的 WAL 内容，校验备份与检查摘要一致，持久化文件、目录和备份收据，再调用 PL `migration::migrate_to_current`。PL 原生事务拥有 schema 11→12 及任务索引回填。成功后重新检查 schema、完整性和原始表摘要，再提交完成收据。报告通过临时文件、同步和原子 rename 更新。

每库独立提交。失败库的原生步骤回滚，后续库不处理；已完成的库不自动回滚。若进程在 schema 提交后、完成收据前中断，备份收据仍在，重新 `check` 可判定库的实际版本。修复失败原因后使用新的备份目录再次执行，当前版本的库只检查。工具从不覆盖旧备份。

工具产生的逐库备份不替代第 1 步的成套备份。回退前停服，按记录的数据时间点成套恢复 server、产品库、会话与环境；不能把旧二进制直接指向 schema 12，不能混合恢复不同时间点的数据。已有新业务写入时，恢复旧备份会放弃该时间点之后的写入，需先明确取舍。

## 验证

本轮证据保存在执行机 `/home/zhourui/.codex/artifacts/mai-pl-mainline-20261016-de4ad609`。历史共享参考源码验收记录保持原样；本文件只记录本次 PL 主线适配。

- 命令 API 四处编译失败先留证，再按新契约适配。捕获计费和头部上限的确定性回归先出现 2 项失败，修复后 3 项全部通过，覆盖换行、无换行、UTF-8/非 UTF-8、分流副本及幂等片段修复。
- 迁移覆盖原始数据保留、备份不覆盖、当前版本重复执行、任务 cold read、身份及 symlink 拒绝、更旧 schema 拒绝、未初始化空库保留及正常原生初始化、全目录锁、PL 每库 lease，以及三库批次中间损坏导致的事务回滚和后续停止。
- `cargo test --locked --workspace -- --test-threads=1`：801 通过、2 忽略。忽略项分别需要目标机 Docker 恢复竞态环境和真实模型凭据，不计为通过。首次默认并行运行在两项临时可执行脚本测试遇到 `ETXTBSY`，原始失败日志保留，没有在产品中添加重试或延迟。
- 前端 typecheck、lint、128 项 Vitest 和 build 通过。Playwright 使用仓库现有 HTTP 夹具，桌面 1440×900、平板 820×1180、手机 390×844 共 30 项通过，覆盖 References 编辑、保存时继续编辑、SSE refetch、各项目草稿和 CAS 冲突；截图存入外部证据目录。Browser plugin not available，使用常规 Playwright。Vite 日志中的未夹具化 `/agents/agent-1` 代理请求连接本地未运行的 8080 被拒绝，此结果不能作为真实 server HTTP 联调通过的证明。
- `cargo fmt --all -- --check`、安装更新脚本回归和 `git diff --check` 通过。`cargo clippy --locked --workspace --all-targets` 通过；Rust 1.99 对既有 GitHub `async_trait` 实现报 10 项 `double_must_use` 警告，本轮没有新增警告或添加抑制。

## 新提交实机结果

目标机 `runner@10.3.10.194`，验收根 `/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources`。本次不升级生产数据或重启生产服务。

- 锁定依赖的 release 构建通过，server、迁移工具及 Runtime 驱动上传前后 SHA256 一致。发布包包含 server、会话与产品库两个迁移工具和内部 `SHA256SUMS`。
- 仅对隔离 `data/sessions` 执行原生迁移：42 个 schema 11 库全部到 12，回填 270 个任务索引；四张原始表摘要一致，隔离产品库文件 SHA256 未变。备份和持久报告在验收根 `pl-backup-8fd235ac-20261016/`，执行机报告为 `remote-session-migration.json`。
- 真实 Docker 命令限制验收 14 通过、0 失败、0 阻塞。无匹配保留退出码 1；超时为 timedOut；模型视图恰好 32768 字节并保留 head/tail；超限为明确 output failure。全部实际宿主捕获文件合计 67092548 字节，小于 67108864 字节；超时及超限后均无标记进程残留。证据 `runtime-channels-limits.json`，对应远端原始报告 `evidence/runtime-channels-limits-20261016T084951Z.json`。
- 另建全新独立 daemon 与数据目录启动同一 server：`/health`、根 HTML、`/projects`、`/agents`、`/mcp-servers`、`/reference-sources` 均 HTTP 200，`/events/product` 建立 text/event-stream 连接。运行中维护工具被目录 lease 拒绝；服务正常排空退出后检查成功。此项验证新实例 HTTP 路由与 SSE 建连，不代表真实模型调用或旧会话的 HTTP 端到端验收。证据 `http-smoke.json`、`http-maintenance-after-stop.json` 和 `http-server-journal.log`。

| 产物 | SHA256 |
|---|---|
| mai-server | `1b79a703e55c03a978617182107dff2df4e08b25637b56a39e1c7a42193e97fb` |
| mai-migrate-pl-sessions | `d05ab83779dfbebd09f31475ee21aed10fb18677bfea325459606ba091ab86f9` |
| mai-migrate-references | `4056187dc0621d0e7aaac2ef701e6b172bf7365aea66df4309cdb4021e925932` |
| shared_references_runtime | `a5eadaa36574b29eb1f7d5b77dd3578ea267da25bccf267047c11435ad52bc60` |

完整租约通道验收 `evidence/runtime-channels-20261016T085855Z.json`：39 通过、0 失败、0 阻塞，1 项内部 API 边界说明。覆盖上传、后台命令、真实 stdin 回显、精确任务身份、wait/get_tool_task 完整载荷、换代后旧租约拒绝、恢复原引用以及重建 Runtime 后旧句柄拒绝。执行机副本为 `runtime-channels.json`。没有公开内部 stat_path/read_bytes API 来迎合验收；这两项由现有文件工具间接覆盖。

Gate 全量验收 `evidence/runtime-gate-20261016T090542Z.json`：25 通过、0 失败、0 阻塞。覆盖活动 Turn 的旧 generation 固定、WhenIdle 期间后续输入等待、真实挂载与下一 Turn 一起切换、StopAndApply 的精确取消、下一输入保留，以及恢复原引用后的工具调用。执行机副本为 `runtime-gate.json`。

先完成带验收驱动的 release 构建和上述实机验证，再按正式发布命令统一构建 server 与两个迁移工具。前述 42 库迁移和首轮 HTTP 使用的校验和保留在 `SHA256SUMS-8fd235ac`；下表列出最终发布三件套和独立 Runtime 验收驱动。正式包的 HTTP/SSE、维护锁和停服后维护复核全部通过；从原始备份选取数据最多的旧会话库复演迁移，134 个任务索引与原 42 库验收结果完全一致，原始数据摘要保持一致。结果记录在 `release-package-probe.json`。所有本轮临时服务和 daemon 在验收后停止，保留数据、备份及证据，不删除旧验收资源。
