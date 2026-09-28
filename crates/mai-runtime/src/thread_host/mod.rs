//! mai 产品层交给 PL Thread 内核的装配输入端口。
//!
//! 本目录只承载“产品已经解析完成、可以直接交给 PL core”的输入：新 Thread 的静态初始指令、产品
//! 资源字节读端口、工具装配请求，以及进程级的 fatal 上报通道。这里不读取配置文件、不访问会话
//! SQLite，也不保存任何 `AgentSnapshot` 兼容层。
//!
//! Thread 的实际装配顺序由 [`crate::thread_kernel`] 负责，严格遵循 PL `design/14` 与
//! `design/16`：先绑定 context preparation、资源、容量、工具与冷存储，再提交扩展和初始 context；
//! 恢复路径只消费 core 的 typed checkpoint，绝不用旧数据重建新的 checkpoint。

mod agents;
mod fatal;
mod instructions;
mod route;
mod tools;

pub(crate) use agents::MaiThreadCollaboration;
pub(crate) use fatal::wait_for_fatal;
pub(crate) use instructions::{ThreadSeed, thread_seed};
pub(crate) use route::{ProductRoute, resolve_agent_route};
pub(crate) use tools::{
    ThreadToolAssembly, ThreadToolRequest, assemble_thread_tools, child_workspace_assignment,
};
