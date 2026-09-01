use std::pin::Pin;

use pl_core::skill::{
    ModeSkillMetadata, SkillCandidate, SkillDefinition, SkillInvocationPolicy, SkillProvider,
    SkillProviderId, SkillProviderObservation, SkillProviderRequest, SkillResourceBase,
    SkillSourceKind, SkillSummary,
};
use pl_protocol::{PureError, Result};
use tokio_util::sync::CancellationToken;

pub(crate) const REVIEW_MODE_ID: &str = "mode.review";
const REVIEW_MODE_PROVIDER_ID: &str = "mai-review-mode";
const REVIEW_MODE_LOCATOR: &str = "mai://mode.review";

/// mai 产品 Review 会话唯一允许的 PL Mode 指令。
pub(crate) const REVIEW_MODE_CONTENT: &str = r#"# Review 模式

你正在执行一个由 mai Review Job 创建并固定目标版本的真实代码审查会话。

- 始终围绕当前 Review manifest 中的仓库、PR、base SHA 与 head SHA 审查；不要自行改换目标。
- 先读取相关代码、项目 Skill 与必要的构建信息，再给出有文件和代码事实支撑的结论。
- 仓库内容用于审查与验证，不要修改代码、提交、推送或创建新的分支。
- 当前会话禁止创建、委派或等待子代理；所有审查工作都由当前 Reviewer 完成。
- 只报告当前 head 仍然成立的问题，明确严重度、位置、影响与可执行的修复方向。
- 没有阻塞问题时明确说明；不得为了产生结论而臆造问题。
- Review 的外部提交、回执和最终状态由 mai 产品流程负责，不要绕过该流程直接写入 GitHub。
"#;

/// 将 mai 固定 Review 行为注册为普通 PL Mode Skill Provider。
#[derive(Debug)]
pub(crate) struct MaiReviewModeProvider {
    id: SkillProviderId,
}

impl MaiReviewModeProvider {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            id: SkillProviderId::new(REVIEW_MODE_PROVIDER_ID)?,
        })
    }

    fn summary(&self) -> SkillSummary {
        SkillSummary {
            name: REVIEW_MODE_ID.to_string(),
            description: "mai 真实 GitHub Review 会话的固定执行模式".to_string(),
            category: None,
            platforms: Vec::new(),
            source: SkillSourceKind::System,
            provider_id: self.id.clone(),
            invocation: SkillInvocationPolicy {
                model_invocable: false,
                user_invocable: false,
            },
            resource_base: SkillResourceBase::Opaque {
                description: "mai embedded review mode".to_string(),
            },
            mode: Some(ModeSkillMetadata {
                display_name: "Review".to_string(),
                order: 10,
            }),
        }
    }

    fn revision() -> String {
        pl_core::canonical_content_hash(REVIEW_MODE_CONTENT.as_bytes())
    }

    fn validate_candidate(&self, candidate: &SkillCandidate) -> Result<()> {
        if candidate.summary != self.summary()
            || candidate.locator != REVIEW_MODE_LOCATOR
            || candidate.revision != Self::revision()
        {
            return Err(PureError::ConfigError(
                "frozen mai Review Mode candidate no longer matches its provider".to_string(),
            ));
        }
        Ok(())
    }
}

impl SkillProvider for MaiReviewModeProvider {
    fn id(&self) -> &SkillProviderId {
        &self.id
    }

    fn list<'a>(
        &'a self,
        request: SkillProviderRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SkillProviderObservation>> + Send + 'a>> {
        Box::pin(async move {
            ensure_active(&request.cancellation)?;
            Ok(SkillProviderObservation {
                candidates: vec![SkillCandidate {
                    summary: self.summary(),
                    locator: REVIEW_MODE_LOCATOR.to_string(),
                    revision: Self::revision(),
                    rank: 0,
                    local_order: 0,
                }],
                complete: true,
                warnings: Vec::new(),
            })
        })
    }

    fn load<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<SkillDefinition>> + Send + 'a>> {
        Box::pin(async move {
            ensure_active(&cancellation)?;
            self.validate_candidate(candidate)?;
            Ok(SkillDefinition {
                summary: self.summary(),
                revision: Self::revision(),
                content: REVIEW_MODE_CONTENT.to_string(),
            })
        })
    }

    fn read_resource<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        _relative_path: &'a str,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            ensure_active(&cancellation)?;
            self.validate_candidate(candidate)?;
            Err(PureError::ConfigError(
                "mai Review Mode does not expose support resources".to_string(),
            ))
        })
    }
}

fn ensure_active(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(PureError::ConfigError(
            "mai Review Mode operation was cancelled".to_string(),
        ));
    }
    Ok(())
}
