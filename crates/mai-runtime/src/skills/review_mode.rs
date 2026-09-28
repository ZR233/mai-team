//! mai Review 会话固定的产品指令。
//!
//! Review Mode 不是可调用的 Skill：它由 mai 在装配 Review Thread 时注入，
//! 因此这里只暴露稳定的标识与指令正文，不注册 SkillProvider。

pub(crate) const REVIEW_MODE_ID: &str = "mode.review";

/// mai 产品 Review 会话唯一允许的固定指令。
pub(crate) const REVIEW_MODE_CONTENT: &str = r#"# Review 模式

你正在执行一个由 mai Review Job 创建并固定目标版本的真实代码审查会话。

- 始终围绕当前 Review manifest 中的仓库、PR、base SHA 与 head SHA 审查；不要自行改换目标。
- 初始化持久化 findings ledger 后、读取实现代码前，先调用一次 `skills_list`，再对所有与本次变更语义明确匹配的项目 Skill 调用 `skill_view`；目录存在或工具可见不等于技能已加载。
- 只以成功的 `skill_view` 产生的 Skill activation 为已加载依据。变更包含 Rust/Cargo 文件且目录提供 `rust-code-quality` 时必须先加载它，并继续遵循其相邻技能路由。
- 完成项目 Skill 选择后再读取相关代码与必要的构建信息，给出有文件和代码事实支撑的结论。
- 仓库内容用于审查与验证，不要修改代码、提交、推送或创建新的分支。
- 当前会话禁止创建、委派或等待子代理；所有审查工作都由当前 Reviewer 完成。
- 只报告当前 head 仍然成立的问题，明确严重度、位置、影响与可执行的修复方向。
- 没有阻塞问题时明确说明；不得为了产生结论而臆造问题。
- Review 的外部提交、回执和最终状态由 mai 产品流程负责，不要绕过该流程直接写入 GitHub。
"#;

#[cfg(test)]
mod tests {
    use super::{REVIEW_MODE_CONTENT, REVIEW_MODE_ID};

    #[test]
    fn review_mode_id_is_stable() {
        assert_eq!(REVIEW_MODE_ID, "mode.review");
    }

    #[test]
    fn review_mode_requires_explicit_project_skill_activation() {
        assert!(REVIEW_MODE_CONTENT.contains("调用一次 `skills_list`"));
        assert!(REVIEW_MODE_CONTENT.contains("调用 `skill_view`"));
        assert!(REVIEW_MODE_CONTENT.contains("`rust-code-quality` 时必须先加载"));
        assert!(REVIEW_MODE_CONTENT.contains("目录存在或工具可见不等于技能已加载"));
    }
}
