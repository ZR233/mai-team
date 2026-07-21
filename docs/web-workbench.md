# Mai Web 工作台

## 技术边界

Mai Web 是 Vite + React + TypeScript 单页应用，使用官方 shadcn/ui 组件源、Tailwind
语义 token、React Router、TanStack Query 和 Zustand。`App` 只组合路由、providers 与 shell；
业务分别位于 chat、projects、providers、settings 和 event transport 模块。

低频产品实体由 TanStack Query 缓存；当前可见 session 的 timeline、turn、Todo、context 和
interaction 由 normalized Zustand store 保存。高频 delta 只订阅受影响 selector，并按
animation frame 合批，不能使整个工作台重渲染。

## 事件连接

浏览器始终最多持有两个 EventSource：

- `/events/product`：应用生命周期内常驻，更新或精确失效项目、任务、review、provider、
  settings 和资源 query。
- `/sessions/{sessionId}/events`：只为当前路由选中的 session 建立；切换时立即关闭旧连接。

session stream 首帧为 snapshot 或 durable replay。客户端用 generation 隔离旧连接，按 session
durable sequence 去重，按 part revision 应用 transient delta。`ResyncRequired`、revision gap 或
协议不变量失败会关闭连接并以无 cursor 重订阅，不能通过整页刷新修复。

## 视觉系统

实现以已批准的工作台和设置页概念稿为视觉事实源：真白与冷灰表面、钴蓝主操作、青绿色
健康状态、细边框、6–8px 圆角和极少阴影。只实现浅色主题，不使用渐变、常驻右侧 inspector、
大面积 dashboard card 或装饰性 badge。

桌面使用全局 nav rail、资源 sidebar 和主工作区；平板把资源栏放入 Sheet；手机使用顶部导航
与 Sheet，但保留查看、发送、审批和设置主路径。timeline 使用开放阅读流，只有 tool、plan、
interaction 等结构化内容使用轻量边界。

## 功能完整性

重构必须保留 environment/agent/session、project/task/review、provider/model catalog、roles、
instructions、skills、MCP、Web Search、Git account、GitHub App/relay 及所有现有 CRUD 和审批。
provider/model/reasoning 继续只消费服务端 catalog，不在 React 代码中加入厂商或模型 ID 分支。

React 深链与 JSON API 会复用 `/projects`、`/tasks`、`/providers`、`/settings` 路径前缀。
边界通过 HTTP 内容协商而不是复制路由解决：文档导航的 `Accept: text/html` 返回嵌入的
`index.html`，Web API client 明确发送 `Accept: application/json`，EventSource 继续使用
`text/event-stream`。Vite 开发代理执行同一规则，因此直接刷新任意工作台深链与生产行为一致。

## 验证

Vitest/Testing Library 验证 reducer、query cache 与订阅生命周期；Playwright 验证桌面、平板、
手机核心工作流和视觉回归。最终浏览器截图必须与批准概念稿通过 `view_image` 直接对照。
