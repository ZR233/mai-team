//! mai 产品的 Thread 资源字节存储。
//!
//! PL core 只通过 [`pl_core::context::ResourceAccess`] 读取二进制资源，工具则通过
//! [`pl_tool::exec::CommandOutputArchive`] 保留完整命令输出，并通过
//! [`pl_tool::media::ToolMediaHost`] 保留 MCP 返回的模型媒体（image/audio/blob）。本目录把
//! 这些产品责任收在一个内容寻址存储中：字节的物理位置、身份生成和持久化细节都留在 mai 产品
//! 层，PL core 只消费 `ResourceReference`，引用各自携带自己的 MIME。
//!
//! 存储根由调用方注入：`artifact_files_root.join("thread-resources")`。这里不读取配置、不依赖
//! Studio 产品 crate，也不把模型、Thread 或调用方提供的字符串直接当作文件路径。

mod store;

pub(crate) use store::{MaiResourceStore, MaiResourceStoreError, thread_resources_root};
