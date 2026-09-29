//! 产品 Thread 的 MCP 工具装配。
//!
//! mai 只负责把本 Agent 容器已经冻结的 MCP generation 转成 PL Thread 独占的工具实例；连接、
//! 探测、命名冲突、health 与工具发现都由 `pl_tool::mcp` 与 [`crate::mcp::ContainerMcpRuntime`]
//! 提供。每个注册项持有同一 generation 的 lease 克隆，因此一次装配只能转移给一个 Thread。
//!
//! # 媒体保留
//! PL 的 MCP 工具通过 [`pl_tool::media::ToolMediaHost`] 保留二进制结果。mai 把它委托给同一个
//! 内容寻址存储 [`crate::thread_resources::MaiResourceStore`]：原始字节按摘要落盘，引用携带
//! `image/png`、`audio/mpeg`、`application/pdf` 等原始 MIME。模型投影（图像归一化与 attachment
//! Opaque payload）仍完全由 PL 负责，这里只做存储与 `RetainedToolMedia` 装配。

use std::sync::Arc;

use pl_core::{
    context::ContextContent,
    tool::opaque::{Registration, ToolError},
};
use pl_tool::mcp::resources::{McpResourceToolKind, ThreadMcpResourceTool};
use pl_tool::mcp::thread::ThreadMcpTool;
use pl_tool::media::{RetainedToolMedia, ToolMedia, ToolMediaHost, ToolMediaKind};

use crate::state::AgentRecord;
use crate::thread_resources::{MaiResourceStore, MaiResourceStoreError, thread_resources_root};
use crate::{AgentRuntime, Result, RuntimeError};

/// 把本 Agent 容器当前冻结的 MCP generation 装配成 Thread 独占的注册项。
///
/// 容器 MCP runtime 尚未驻留时返回显式错误：调用方必须先确保 Agent 容器存在，不能因为
/// “没有 MCP 工具”而静默发布一个缺少已配置能力的 Thread。
pub(super) async fn registrations(
    runtime: &Arc<AgentRuntime>,
    agent: &AgentRecord,
) -> Result<McpThreadTools> {
    let Some(mcp) = agent.mcp.read().await.clone() else {
        return Err(invalid(format!(
            "agent {} has no resident MCP runtime; its container must be started before Thread tool \
             assembly",
            agent.summary.read().await.id
        )));
    };
    let lease = mcp.handle().acquire_turn_lease().await.map_err(|error| {
        invalid(format!(
            "failed to freeze MCP tools for this Thread: {error}"
        ))
    })?;
    let active_servers = lease.server_ids().to_vec();
    let media = Arc::new(MaiToolMediaHost {
        store: MaiResourceStore::new(thread_resources_root(&runtime.artifact_files_root)),
    });
    let mut registrations = Vec::with_capacity(lease.tools().len());
    for descriptor in lease.tools() {
        let tool = ThreadMcpTool::new(lease.clone(), &descriptor.exposed_name, media.clone())
            .map_err(|error| {
                invalid(format!(
                    "failed to bind MCP tool `{}`: {error}",
                    descriptor.exposed_name
                ))
            })?;
        let declaration =
            pl_model::runtime::thread_tool_declaration(&tool.declaration()).map_err(|error| {
                invalid(format!(
                    "failed to encode MCP tool `{}` declaration: {error}",
                    descriptor.exposed_name
                ))
            })?;
        registrations.push(tool.registration(declaration).map_err(|error| {
            invalid(format!(
                "failed to register MCP tool `{}`: {error}",
                descriptor.exposed_name
            ))
        })?);
    }
    if lease.has_resources() {
        for kind in McpResourceToolKind::all() {
            let tool = ThreadMcpResourceTool::new(lease.clone(), *kind, media.clone());
            let declaration = pl_model::runtime::thread_tool_declaration(&tool.declaration())
                .map_err(|error| {
                    invalid(format!(
                        "failed to encode MCP resource tool `{}` declaration: {error}",
                        kind.name()
                    ))
                })?;
            registrations.push(tool.registration(declaration).map_err(|error| {
                invalid(format!(
                    "failed to register MCP resource tool `{}`: {error}",
                    kind.name()
                ))
            })?);
        }
    }
    Ok(McpThreadTools {
        active_servers,
        registrations,
    })
}

/// 同一个冻结 generation 产生的工具注册项与产品展示事实。
pub(super) struct McpThreadTools {
    pub(super) active_servers: Vec<String>,
    pub(super) registrations: Vec<Registration>,
}

/// mai 的 MCP 媒体保留端口。
#[derive(Debug)]
struct MaiToolMediaHost {
    store: MaiResourceStore,
}

impl ToolMediaHost for MaiToolMediaHost {
    async fn retain(&self, media: ToolMedia) -> std::result::Result<RetainedToolMedia, ToolError> {
        let ToolMedia {
            kind,
            bytes,
            media_type,
            model_image,
        } = media;
        // 原始字节始终保留；身份只来自字节摘要，media type 只进入返回引用。
        let reference = self
            .store
            .retain_bytes(bytes.clone(), &media_type)
            .await
            .map_err(MaiResourceStoreError::tool_error)?;
        let context = match model_image {
            Some(image) => {
                // 图像只有在字节与 MIME 都未变化时才复用同一对象，否则把模型投影单独保留；投影的
                // attachment 编码由 PL 提供，这里不复制它的媒体编码逻辑。
                let projected =
                    if image.media_type == media_type && image.bytes.as_ref() == bytes.as_ref() {
                        reference.clone()
                    } else {
                        self.store
                            .retain_bytes(image.bytes, &image.media_type)
                            .await
                            .map_err(MaiResourceStoreError::tool_error)?
                    };
                vec![
                    pl_model::runtime::attachment_content(
                        projected,
                        pl_protocol::AttachmentModality::Image,
                    )
                    .map_err(ToolError::new)?,
                ]
            }
            None => {
                let mut context = vec![ContextContent::Resource {
                    reference: reference.clone(),
                }];
                if matches!(kind, ToolMediaKind::Image) {
                    context.push(ContextContent::Text {
                        text: Arc::from(
                            "Image retained as a resource; this prepared model does not advertise \
                             image input.",
                        ),
                    });
                }
                context
            }
        };
        Ok(RetainedToolMedia { reference, context })
    }
}

fn invalid(message: String) -> RuntimeError {
    RuntimeError::InvalidInput(message)
}

#[cfg(test)]
mod tests {
    use pl_core::context::ResourceReader;
    use pl_tool::media::PreparedToolImage;
    use pretty_assertions::assert_eq;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn host_in(temp: &tempfile::TempDir) -> MaiToolMediaHost {
        MaiToolMediaHost {
            store: MaiResourceStore::new(thread_resources_root(temp.path())),
        }
    }

    #[tokio::test]
    async fn retains_blob_and_reports_its_declared_mime() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let host = host_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"%PDF-1.7 mcp blob".to_vec());

        let retained = host
            .retain(ToolMedia {
                kind: ToolMediaKind::Blob,
                bytes: Arc::clone(&bytes),
                media_type: "application/pdf".to_string(),
                model_image: None,
            })
            .await
            .expect("blob retained");

        assert_eq!(retained.reference.media_type(), "application/pdf");
        assert_eq!(retained.reference.byte_len(), bytes.len() as u64);
        retained
            .reference
            .verify(&bytes)
            .expect("reference binds the exact bytes");
        assert_eq!(
            retained.context,
            vec![ContextContent::Resource {
                reference: retained.reference.clone(),
            }]
        );

        let read = ResourceReader::read(&host.store, retained.reference, CancellationToken::new())
            .await
            .expect("retained media reads back");
        assert_eq!(&read[..], &bytes[..]);
    }

    #[tokio::test]
    async fn image_without_a_model_projection_becomes_a_resource_with_a_note() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let host = host_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"png bytes".to_vec());

        let retained = host
            .retain(ToolMedia {
                kind: ToolMediaKind::Image,
                bytes,
                media_type: "image/png".to_string(),
                model_image: None,
            })
            .await
            .expect("image retained as a resource");

        assert_eq!(retained.context.len(), 2);
        assert!(matches!(
            &retained.context[0],
            ContextContent::Resource { reference } if reference == &retained.reference
        ));
        assert!(matches!(&retained.context[1], ContextContent::Text { .. }));
    }

    #[tokio::test]
    async fn image_model_projection_is_retained_as_a_pl_attachment() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let host = host_in(&temp);
        let original: Arc<[u8]> = Arc::from(b"raw mcp image bytes".to_vec());
        let projected: Arc<[u8]> = Arc::from(b"normalized model image".to_vec());

        let retained = host
            .retain(ToolMedia {
                kind: ToolMediaKind::Image,
                bytes: Arc::clone(&original),
                media_type: "image/png".to_string(),
                model_image: Some(PreparedToolImage {
                    bytes: Arc::clone(&projected),
                    media_type: "image/png".to_string(),
                }),
            })
            .await
            .expect("image and projection retained");

        // 返回引用永远绑定原始字节；模型投影单独保留，两者可以不同。
        retained
            .reference
            .verify(&original)
            .expect("returned reference binds the original bytes");
        let ContextContent::Opaque { payload } = &retained.context[0] else {
            panic!(
                "model projection must be an opaque attachment, got {:?}",
                retained.context
            );
        };
        let attachment = pl_model::runtime::decode_attachment(payload)
            .expect("attachment projection decodes")
            .expect("mai emits the PL attachment projection");
        assert_eq!(attachment.modality, pl_protocol::AttachmentModality::Image);
        assert_eq!(attachment.reference.media_type(), "image/png");
        attachment
            .reference
            .verify(&projected)
            .expect("projection is retained under its own digest");
        assert_ne!(
            attachment.reference, retained.reference,
            "different bytes must not share one identity"
        );
    }

    #[tokio::test]
    async fn unchanged_image_projection_reuses_the_single_object() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let host = host_in(&temp);
        let bytes: Arc<[u8]> = Arc::from(b"already-normalized".to_vec());

        let retained = host
            .retain(ToolMedia {
                kind: ToolMediaKind::Image,
                bytes: Arc::clone(&bytes),
                media_type: "image/png".to_string(),
                model_image: Some(PreparedToolImage {
                    bytes: Arc::clone(&bytes),
                    media_type: "image/png".to_string(),
                }),
            })
            .await
            .expect("unchanged projection retained");

        let ContextContent::Opaque { payload } = &retained.context[0] else {
            panic!(
                "model projection must be an opaque attachment, got {:?}",
                retained.context
            );
        };
        let attachment = pl_model::runtime::decode_attachment(payload)
            .expect("attachment projection decodes")
            .expect("mai emits the PL attachment projection");
        assert_eq!(attachment.reference, retained.reference);

        let entries = std::fs::read_dir(temp.path().join("thread-resources"))
            .expect("resource root readable")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("resource entries readable");
        assert_eq!(entries.len(), 1, "an unchanged projection adds no object");
    }

    #[tokio::test]
    async fn malformed_media_type_fails_without_writing() {
        let temp = tempfile::tempdir().expect("isolated resource root");
        let host = host_in(&temp);
        let error = host
            .retain(ToolMedia {
                kind: ToolMediaKind::Blob,
                bytes: Arc::from(b"payload".to_vec()),
                media_type: "not-a-mime".to_string(),
                model_image: None,
            })
            .await
            .expect_err("a malformed MIME is refused");
        assert!(
            error
                .source
                .downcast_ref::<MaiResourceStoreError>()
                .is_some_and(|error| matches!(
                    error,
                    MaiResourceStoreError::UnsupportedMediaType(_)
                )),
            "{error:?}"
        );
    }
}
