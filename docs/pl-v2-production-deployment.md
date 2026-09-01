# PL v2 生产迁移与 Review 验收

本文档只适用于 mai-store schema 31→32 的一次性 PL v2 升级。生产实例为
`mai-server.service`，二进制 `/opt/mai-server/mai-server`，数据目录
`/var/lib/mai-server`。所有时间戳目录均永久保留，不能由普通清理任务删除。

## 进入窗口前

必须记录两仓完整提交、构建产物 SHA256、当前项目的 `auto_review_enabled`、所有非终态
Review Job/Run、reviewer Agent、租约 owner/expiry、PR number 和 head SHA。只有 PL 与 mai-team
远端 CI 均终态成功、release 产物来自记录的 mai-team 提交时才能继续。

通过 `PATCH /projects/{id}` 把所有原本启用的项目设置为：

```json
{ "auto_review_enabled": false }
```

该产品 API 会禁用独立 discovery、停止 Review worker，取消活跃 Job/Run 和 Agent Turn，删除临时 Reviewer，并清空
Review 租约。保存每个被取消目标的 project、PR 和原 head；恢复时必须重新读取 GitHub 最新 head，
不能直接复用旧 SHA。

停服前反复读取项目 Review 列表/详情并核对数据库，直到：

- Job/Run 均进入终态；
- `current_reviewer_agent_id` 为空且没有 Reviewer 产品 Agent；
- 没有有效 lease owner/expiry；
- 没有 `submission_pending` 或 `reconciling`。已有 SubmissionIntent 但无 receipt 时禁止迁移，必须
  先完成 GitHub 对账。

## 停服、回滚物与迁移

执行 `systemctl stop mai-server` 后确认进程退出，WAL 不再增长。创建新的
`/var/lib/mai-server/deploy-backups/<timestamp>-pl-v2/`，完整保存：

- `/opt/mai-server/mai-server`；
- `mai-team.sqlite3` 以及仍存在的 `-wal`、`-shm`；
- `config.toml`；
- `/etc/mai-server/mai-server.env`、systemd unit 和 drop-in；
- 新旧二进制、数据库和配置的 SHA256 清单。

先对停服数据库执行：

```text
mai-migrate --database /var/lib/mai-server/mai-team.sqlite3 --check
```

只有报告确认源 schema 为 31 且没有活跃 Review 后，才执行：

```text
mai-migrate \
  --database /var/lib/mai-server/mai-team.sqlite3 \
  --archive-root /var/lib/mai-server/framework-archives \
  --source-commit <旧 mai-team commit> \
  --target-commit <新 mai-team commit>
```

迁移必须生成 `framework-archives/pl-v2-<timestamp>/manifest.json` 和完整 SQLite backup。逐项核对
manifest 中数据库 SHA256、表行数、源/目标提交、保留的 Agent/Job/Run 数与清理的 Thread/Turn/
Item 数；随后再次运行 `--check`，确认目标 schema 为 32。任一校验失败都不得启动新服务。

## 原子替换与恢复窗口

把精确 release 产物安装为 `/opt/mai-server/mai-server.next`，核对 owner、mode 和 SHA256 后，在
同一文件系统用 rename 原子替换 `/opt/mai-server/mai-server`。启动 `mai-server.service` 后至少
等待一个完整恢复窗口，不因短暂无日志或容器拉取而快速重复重启。

恢复窗口同时检查：

- `GET /health`；
- 项目、Agent、Review Job/Run API；
- `/events/product` 与 `/threads/{id}/events` 首帧；
- journal 中 SQLite、writer、容器、MCP、内存和 panic 事件；
- Web Review 列表/详情；旧 Run 应显示“PL v2 升级离线归档”，不能请求旧 Timeline；
- 首次访问长期 Agent 时按 ID 创建或恢复 v2 Thread，启动时不全量驻留。

## 恢复自动 Review 与真实验收

把窗口前原本启用的项目恢复为 `auto_review_enabled: true`。对每个受控取消目标重新读取最新 PR
head，再调用 `POST /projects/{id}/pull-requests/{pr}/review`。现有“同一 project + PR 单活 Job”
约束负责去重；不要直接写数据库或复活旧 Job。

至少等待一个真实 GitHub Review 完成，且以下事实必须同时成立：

- Job 为 `succeeded`；
- 对应 Run `outcome = review_submitted`；
- receipt ID 能在 GitHub 查询到，receipt event 与 Review event 一致；
- GitHub Review `commit_id` 等于验收时 PR 当前 head；
- Review 正文含 `<!-- mai-review-job:{job_id} -->`，且 marker 中 Job ID 与数据库一致；
- Web 详情、API receipt 和 GitHub 返回的 ID、URL、event、head 完全一致。

健康检查、模拟工具结果或仅完成 Agent Turn 都不能替代这条真实回执。

## 回滚边界

新服务尚未产生 schema 32 业务写入时，可以停止服务并整体恢复旧二进制、旧数据库及配置。
一旦 schema 32 已产生业务写入，只能恢复整份部署前数据库，并明确丢弃部署后的全部写入；禁止
把旧 binary、schema 31 数据、schema 32 WAL 或任一代单表拼接使用。
