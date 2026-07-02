# Mai Team

Mai Team 是一个面向软件项目协作的 AI Agent 工作台。它提供 Web UI、服务端运行时和 Docker 隔离执行环境，让多个 agent 可以围绕聊天任务、项目仓库和 GitHub Pull Request 审查协同工作。

项目当前由 Rust 后端、Vue 前端、SQLite 本地存储、Docker 工作区和可选的 GitHub App Relay 组成，适合部署在个人开发机或团队内部服务器上。

## 主要功能

- **Chat 环境工作区**：在 Web UI 中创建隔离的聊天环境，和具备工具调用能力的 agent 持续对话。
- **项目工作区**：从 GitHub 仓库创建项目，维护项目级仓库缓存和 per-agent 工作区。
- **自动 PR 审查**：启用项目审查后，系统会选择合格 Pull Request，启动临时 reviewer agent 检查 diff、运行本地验证并提交 GitHub review。
- **GitHub App 与 Relay**：支持通过 GitHub App 接收 webhook；`mai-relay` 可部署在公网侧，将 GitHub 事件安全转发给内网 `mai-server`。
- **模型 Provider 管理**：通过 Web UI 配置 OpenAI 兼容接口、Zhipu、DeepSeek、MiMo 等模型 provider，并支持 provider 连通性测试。
- **MCP 与工具调用**：支持配置 MCP server，让 agent 在容器内访问更多工具能力。
- **Skills 系统**：支持系统技能、项目技能检测和会话中的技能激活记录。
- **运行日志与事件流**：通过 SSE 实时推送 agent 状态、消息流、工具调用、上下文压缩、待办事项和错误事件。
- **Artifacts**：agent 可以保存输出文件，Web UI 提供下载入口。

## 项目特点

- **隔离优先**：agent 在 Docker 容器和独立 workspace volume 中执行命令，项目缓存、agent 工作区和 GitHub sidecar 职责分离。
- **面向项目审查的工作流**：PR 选择、队列、reviewer agent 生命周期、review run 结果记录都有明确运行时契约。
- **本地优先的数据模型**：配置、事件、项目、日志和 artifact 索引默认保存在本地 `.mai-team` 数据目录，也可通过 `--data-path` 指定。
- **Web UI 内置发布产物**：`mai-server` 构建时会打包前端静态资源，运行一个二进制即可打开管理界面。
- **内部边界清晰**：workspace 按 crate 拆分协议、模型、运行时、存储、Docker、MCP、server 和 relay，方便按责任演进。

## 架构概览

```text
Web UI (Vue/Vite)
        |
        v
mai-server (Axum API + SSE + embedded frontend)
        |
        +-- mai-runtime: agent、项目、任务、审查循环、工具编排和 pl-model 调用
        +-- mai-store: SQLite 配置、事件、日志、artifact 索引
        +-- mai-docker: agent 容器、workspace volume 和命令执行
        +-- mai-mcp / mai-tools / mai-skills: MCP、工具定义和技能加载
        |
        +-- Docker agent 容器
        +-- GitHub sidecar
        +-- 可选 mai-relay (GitHub App webhook 转发)
```

主要 crate：

- `crates/mai-server`：HTTP API、Web UI 静态资源、SSE、启动配置和服务编排。
- `crates/mai-runtime`：agent 生命周期、项目工作区、PR 审查循环、工具调用、模型请求和 artifact 保存。
- `crates/mai-protocol`：跨 crate 共享的数据结构和事件协议。
- `crates/mai-store`：SQLite 持久化层。
- `crates/mai-docker`：Docker 容器和命令执行封装。
- `crates/mai-relay` / `crates/mai-relay-client`：GitHub App webhook relay 服务及客户端。
- `crates/mai-skills`、`crates/mai-agents`、`crates/mai-tools`、`crates/mai-mcp`：技能、系统 agent 配置、工具 schema 和 MCP 集成。

## 快速开始

### 前置条件

- Rust toolchain，支持 edition 2024。
- Node.js `^20.19.0` 或 `>=22.12.0`，用于开发和构建 Web UI。
- Docker daemon 可用，且运行 `mai-server` 的用户能访问 Docker socket。

### 本地运行 server

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=gpt-5.5
export MAI_BIND_ADDR=127.0.0.1:8080

cargo run -p mai-server -- --data-path .mai-team
```

启动后打开：

```text
http://127.0.0.1:8080/
```

常用环境变量：

```text
MAI_BIND_ADDR=0.0.0.0:8080
OPENAI_API_KEY=
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_MODEL=gpt-5.5
MAI_AGENT_BASE_IMAGE=ghcr.io/zr233/mai-team-agent:latest
MAI_SIDECAR_IMAGE=ghcr.io/zr233/mai-team-sidecar:latest
```

### 前端开发模式

后端运行在 `127.0.0.1:8080` 后，可单独启动 Vite：

```bash
cd crates/mai-server/web
npm run dev
```

Vite 默认会把 API 请求代理到 `http://127.0.0.1:8080`。如需指定后端：

```bash
MAI_WEB_API_TARGET=http://127.0.0.1:8080 npm run dev
```

## 开发命令

Rust：

```bash
cargo test
cargo run -p mai-server
cargo run -p mai-relay
```

Web UI：

```bash
cd crates/mai-server/web
npm run build
npm run test:sse
npm run test:timeline
npm run test:projects
```

## 部署提示

仓库提供 Ubuntu 22.04 / 24.04 x86_64 的安装和更新脚本，可将 `mai-server` 与 `mai-relay` 注册为 systemd 服务。详见：

- [scripts/README.md](scripts/README.md)

`mai-server` 安装后默认使用 `/var/lib/mai-server` 作为数据目录，并通过 `/etc/mai-server/mai-server.env` 读取运行配置。`mai-relay` 需要设置 `MAI_RELAY_TOKEN`，并通过 Settings 页面与 `mai-server` 建立连接。

## 相关文档

- [项目 PR 审查循环契约](docs/project-review-loop.md)
- [模型 Provider 维护说明](docs/model-provider-maintenance.md)
- [安装脚本说明](scripts/README.md)

## 许可证

本项目使用 MIT License。
