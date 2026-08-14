/**
 * Thread timeline 模块公共 API。
 *
 * 投影层（buildTimelineEntries 等）与分区模型属于内部实现，测试直接
 * 引用各子模块；对外只导出 chat 与 review 共用的视图入口。
 */

export { ThreadTimeline, TimelineEntriesView } from "./timeline-view"
