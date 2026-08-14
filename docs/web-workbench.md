# Mai Web 工作台

## 技术边界

Mai Web 是 Vite + React + TypeScript 单页应用，使用官方 shadcn/ui 组件源、Tailwind
语义 token、React Router、TanStack Query 和 Zustand。`App` 只组合路由、providers 与 shell；
业务分别位于 chat、projects、providers、settings 和 event transport 模块。

低频产品实体由 TanStack Query 缓存；每个 PL Thread 的 timeline、turn、Todo、context 和
interaction 由独立 `ThreadStore` 保存。`ThreadStoreRegistry` 以 `ThreadId` 隔离 snapshot、
revision、generation、连接和 delta buffer。高频 delta 只订阅受影响 selector，并按 animation
frame 合批，不能使整个工作台重渲染。

## 事件连接

浏览器持有两类 EventSource：

- `/events/product`：应用生命周期内常驻，更新或精确失效项目、任务、review、provider、
  settings 和资源 query。
- `/threads/{threadId}/events`：每个已挂载的 Thread 各自建立；视图卸载时立即关闭对应连接。

Thread stream 首帧必须是 authoritative snapshot，随后只应用同一 `threadId` 的 notification。
客户端用 generation 隔离旧连接，要求 Thread revision 严格连续，并按 Item revision 应用 delta。
`Lagged`、revision gap、Item ownership 失败或协议不变量失败都会关闭当前连接并重新获取 snapshot；
历史分页只走 `ThreadTurnPage`，不依赖 SSE replay。

## 视觉系统

实现以已批准的工作台和设置页概念稿为视觉事实源：真白与冷灰表面、钴蓝主操作、青绿色
健康状态、细边框、6–8px 圆角和极少阴影。只实现浅色主题，不使用渐变、常驻右侧 inspector、
大面积 dashboard card 或装饰性 badge。

桌面使用全局 nav rail、资源 sidebar 和主工作区；平板把资源栏放入 Sheet；手机使用顶部导航
与 Sheet，但保留查看、发送、审批和设置主路径。timeline 使用开放阅读流：文字消息直接阅读，
连续的 tool 调用按"文字/思考输出之间"的相邻段折叠为单个分组行（默认收起，展示数量、
时长与失败数，运行中显示 Using 与脉动点），单个 tool 调用渲染为紧凑可折叠行；reasoning
默认折叠，流式生成时展开、完成后收起为一行时长摘要。chat timeline 与 review 活动列表
复用同一投影与渲染路径，分组行为一致。

## 功能完整性

重构必须保留 environment/agent/thread、project/task/review、provider/model catalog、roles、
instructions、skills、MCP、Web Search、Git account、GitHub App/relay 及所有现有 CRUD 和审批。
provider/model/reasoning 继续只消费服务端 catalog，不在 React 代码中加入厂商或模型 ID 分支。

React 深链与 JSON API 会复用 `/projects`、`/tasks`、`/providers`、`/settings` 路径前缀。
边界通过 HTTP 内容协商而不是复制路由解决：文档导航的 `Accept: text/html` 返回嵌入的
`index.html`，Web API client 明确发送 `Accept: application/json`，EventSource 继续使用
`text/event-stream`。Vite 开发代理执行同一规则，因此直接刷新任意工作台深链与生产行为一致。

## 验证

Vitest/Testing Library 验证 reducer、query cache 与订阅生命周期；Playwright 验证桌面、平板、
手机核心工作流和视觉回归。最终浏览器截图必须与批准概念稿通过 `view_image` 直接对照。
