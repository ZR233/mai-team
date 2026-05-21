# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.5](https://github.com/ZR233/mai-team/releases/tag/mai-server-v0.1.5) - 2026-05-21

### Added

- 优化 McpAgentManager 的 resource_servers 方法，简化服务器收集逻辑
- 添加 mai-server 的构建、打包和安装脚本，优化服务器发布流程
- 添加项目审查决策字段，优化审查结果处理逻辑和 UI 展示
- 添加 token 使用情况追踪，优化项目审查运行的记录和展示
- implement token usage tracking and tooltip in the UI
- 添加滚动到最新消息按钮，优化聊天和项目工作区的用户体验
- 添加项目审查运行布局测试，优化样式和响应式设计
- 添加聊天容器状态管理功能，优化环境准备和对话管理
- 添加时间轴样式和类名生成函数以增强组件样式
- queue relay PR signals before selection
- 添加项目审查状态“选择中”，更新相关逻辑和界面显示
- 更新依赖项，增强错误处理，优化响应构建逻辑
- 更新工作流以支持 release-plz 管理 GitHub 发布，调整版本号并更新安装脚本参数
- add relay manager and settings management
- Add support for GitHub App event handling and improve user experience in project dialogs
- Implement project review PR selection and queuing
- enhance event streaming and handling in the server and client
- enhance SSE event handling and provider management
- 添加 ProviderTestRequest 和 ProviderTestResponse 结构体，增强提供者测试功能
- 修改活动区域为“agents”，移除不必要的“planner”模板
- add agent logs and tool traces functionality
- 优化参数传递和结构体使用，简化代码逻辑
- add agent profiles and capabilities management
- 增强 reviewer-agent-review-pr 工具，添加对嵌套包的支持，优化 Git 命令错误处理
- 更新 reviewer-agent-review-pr 文档和脚本，增强 GitHub MCP 响应处理逻辑，优化 PR 选择和审核流程
- 添加项目删除功能，增强项目管理界面的用户体验
- 添加 GitHub PR 审查工具的模型脚注自动附加功能，并更新文档说明
- 增强项目审查运行标题显示逻辑，添加失败状态处理
- add project review runs functionality
- 添加缓存和工件文件根路径到 RuntimeConfig，更新 ConfigStore 以支持工件索引目录
- 添加选定代理和会话功能，更新项目详细信息处理
- enhance reviewer agent functionality and UI for automatic PR reviews
- add reviewer agent skill for deep GitHub pull request reviews and update related configurations
- enhance API response handling to support 205 status and improve empty response checks
- refactor session handling and improve default implementations for Git enums
- add turn cancellation functionality for agents
- add skills for reviewing open PRs, managing Starry test cases, and updating std tests
- Add SkillsActivated event handling and related structures for skill activation
- Enhance system skills management with anthropic skills integration and long description support
- Add system skills support and GitHub API integration for PR reviews
- Add git binary configuration and enhance git account management logic
- Enhance Git account management with improved save logic and timeout handling
- Add Git account verification status and update related logic
- Refactor project workspace setup and enhance UI for project progress
- Remove project_path and workspace_path from project structures and update related logic
- Add runtime defaults and repository package management functionality
- Remove GitHub username input and set default label for Git account form
- Enhance Docker workflow and runtime to support sidecar containers
- Introduce Git account management functionality
- add GitHub App settings management and project handling
- 添加 GitHub 设置功能，包括获取和保存个人访问令牌的 API 支持
- add MCP server management functionality
- 优化前端构建流程，添加临时目录和 npm 缓存支持
- enhance skill management with OpenAI metadata support
- 添加对 MiMo 提供者的支持，包括 API 和 Token Plan 选项
- 支持从 artifact_created 事件中提取 agent_id
- add snake game implementation in HTML and JavaScript
- 添加工件管理功能，支持保存和下载工件
- 添加计划审批条，支持审批和请求修订功能
- add request plan revision functionality
- 添加待办事项列表功能，支持更新和显示任务状态
- 移除任务对话框中的标题输入，更新任务创建逻辑以使用初始消息
- 优化任务创建流程，添加提交状态管理和加载指示器
- 添加 Dockerfile 和 GitHub Actions 工作流以构建和推送代理镜像，更新默认基础镜像
- Introduce task management features
- 移除研究代理相关字段，更新代理配置，优化数据库模式
- 添加 Explorer 角色支持，更新相关配置和界面
- 添加代理角色支持，重构代理配置，优化界面和样式
- 重构 ProviderGrid 组件，优化样式和布局，增强可读性和用户体验
- 添加 Docker 镜像支持，增强代理创建功能
- 添加代理容器管理功能，支持从父容器克隆和删除代理及其子代理
- 增强错误处理，添加错误标题和状态标签
- 优化条件判断和默认值设置，简化代码逻辑
- add agent configuration management
- add new components for chat interface and agent management
- 添加上下文使用情况结构体及其在代理详细信息中的支持
- add context compaction feature with event handling
- 添加思考深度控制，优化代理模型更新逻辑
- add reasoning effort support to agent management
- 增强npm依赖管理，确保前端依赖完整性
- add session management for agents
- Enhance provider management and model configuration
- 添加管理容器功能，优化代理容器的清理和管理逻辑
- *(runtime)* add tool trace functionality to retrieve tool call history and metadata
- add agent management components and functionality
- 添加代码高亮和Markdown支持，优化UI交互体验
- initialize Vue.js application with main entry point and styles
- add mai-store crate for managing provider configurations and MCP servers
- add mai-server, mai-skills, and mai-tools crates with initial implementations

### Fixed

- make release-plz git-only updates packageable

### Other

- [codex] FreeRootSandbox 与 gh sidecar 工作区重构 ([#24](https://github.com/ZR233/mai-team/pull/24))
- Refactor project review selector and related components
- Add relay self-update flow ([#20](https://github.com/ZR233/mai-team/pull/20))
- Merge pull request #18 from ZR233/fix/chat
- Merge remote-tracking branch 'origin/design/task-environments-ui'
- Add task environment refactor concept diagram
- enhance token usage tracking and improve chat completions handling
- run project reviews in agent clones
- Add regression tests near implementation modules and introduce pretty_assertions ([#16](https://github.com/ZR233/mai-team/pull/16))
- Refactor server assets/events and system resource boundaries ([#14](https://github.com/ZR233/mai-team/pull/14))
- Refactor GitHub APIs: introduce GithubService to centralize relay/local dispatch ([#15](https://github.com/ZR233/mai-team/pull/15))
- Refactor artifacts/tasks: introduce ArtifactService and download response type ([#13](https://github.com/ZR233/mai-team/pull/13))
- Refactor provider APIs into ProviderService/ProviderTester and fix all clippy warnings ([#12](https://github.com/ZR233/mai-team/pull/12))
- *(server)* centralize config sources
- Refactor mai-server helpers into focused support modules ([#11](https://github.com/ZR233/mai-team/pull/11))
- Refactor mai-server: extract bootstrap and HTTP router from main.rs ([#10](https://github.com/ZR233/mai-team/pull/10))
- Merge remote-tracking branch 'origin/project-host-workspace-isolation'
- refactor chat model picker
- Merge branch 'main' of ssh://github.com/ZR233/mai-team
- 添加 reqwest 依赖并新增 provider_smoke 测试文件
- 重构代码，调整导入顺序以提高可读性
- 添加 mai-relay-client 模块及其依赖，重构 GitHub 应用后端逻辑
- Refactor import statements in relay.rs to separate GithubAppBackend from mai_runtime module
- improve Settings and GitHub App page UI
- add @playwright/test as a dev dependency
- enhance timeline component structure and styling
- Refactor code structure for improved readability and maintainability
- Merge remote-tracking branch 'origin/dev/gh-app'
- Merge branch 'main' of ssh://github.com/ZR233/mai-team
- Refactor model client and provider interaction
- Refactor artifact download response and enhance filename handling
- Refactor reviewer-agent-review-pr skill and add review_pr_helper script
- update Cargo.toml to include additional features for rmcp
- Remove duplicate snake.html files and enhance artifact handling in the mai-runtime and mai-store modules
- Refactor reasoning effort handling in the MAI protocol
- Refactor ConfigStore to use toasty for database management and migrate to async operations
