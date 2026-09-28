//! 新 Thread 的静态初始指令。
//!
//! PL core 的模型可见指令保存在 Thread 自己的 context 里，并在装配时一次性提交；恢复路径不允许
//! 再用当前配置覆盖旧内容，因此只有 checkpoint 不存在的新 Thread 才会调用本模块。
//!
//! 本模块只固化 Thread 生命周期内稳定的指令：mai 运行时基础指令、Agent 的 system prompt 与
//! `MaiConfig.instructions` 覆盖。每个 turn 才成立的事实（MCP 工具清单、Skill 激活、review
//! manifest、工作区动态说明）不属于这里，由 turn 装配方在模型请求边界处理，避免把一次性快照
//! 重复写进同一个 Thread。Review Thread 的固定产品指令也在这里随新 Thread 一次性写入；
//! 恢复路径复用既有 checkpoint，不会再注入第二份。

use std::{collections::BTreeMap, sync::Arc};

use mai_protocol::AgentSummary;
use pl_core::context::{ContextContent, ContextRecord, ContextSource, OpaquePayload};

use crate::config::MaiInstructionsConfig;

/// `build_instructions` 中“按 turn 变化的 MCP 工具清单”章节标题。
///
/// 初始指令是 Thread 级不可变快照，不能固化一份之后会失效的 MCP 目录；该章节由 `mai runtime`
/// 基础指令模块统一附加，凡出现在这个标记之后的内容都被裁剪。
const DYNAMIC_MCP_SECTION: &str = "\n\n## MCP Tools";

/// 一个新 Thread 的一次性装配种子。
#[derive(Debug, Default)]
pub(crate) struct ThreadSeed {
    /// 初始 context 记录；按 source 顺序提交，全部通过 core 的调用配对校验。
    pub(crate) context: Vec<ContextRecord>,
    /// 初始扩展；mai 目前不冻结任何 core 需要解释的扩展。
    pub(crate) extensions: BTreeMap<String, OpaquePayload>,
}

/// 为新 Thread 生成静态初始指令。
///
/// `system_prompt` 是产品持久化的 Agent 提示词；为空时只保留 mai 运行时基础指令（与旧产品对
/// 无提示词根 Agent 的行为一致），不再按角色补写默认提示词，避免同一 Thread 出现两套角色语义。
pub(crate) fn thread_seed(
    summary: &AgentSummary,
    system_prompt: Option<&str>,
    instructions: &MaiInstructionsConfig,
) -> ThreadSeed {
    let thread_id = summary.id.to_string();
    let mut context = Vec::new();
    let mut index = 0_usize;

    let runtime_instructions = crate::instructions::build_instructions(system_prompt, &[]);
    let runtime_instructions = strip_dynamic_mcp_section(runtime_instructions);
    if !runtime_instructions.trim().is_empty() {
        context.push(instruction_record(
            &thread_id,
            index,
            "runtime",
            runtime_instructions,
        ));
        index += 1;
    }
    if summary.review_run_id.is_some() {
        context.push(instruction_record(
            &thread_id,
            index,
            "review-mode",
            crate::skills::REVIEW_MODE_CONTENT.to_string(),
        ));
        index += 1;
    }
    if !instructions.base.trim().is_empty() {
        context.push(instruction_record(
            &thread_id,
            index,
            "config-base",
            instructions.base.clone(),
        ));
        index += 1;
    }
    if !instructions.developer.trim().is_empty() {
        context.push(instruction_record(
            &thread_id,
            index,
            "config-developer",
            instructions.developer.clone(),
        ));
    }
    if !instructions.user.trim().is_empty() {
        context.push(ContextRecord {
            id: format!("runtime:{thread_id}:config-user"),
            turn_id: None,
            source: ContextSource::Runtime {
                source_id: "mai.config.user".to_string(),
            },
            content: vec![ContextContent::Text {
                text: Arc::from(instructions.user.as_str()),
            }],
            tool_calls: Vec::new(),
        });
    }

    ThreadSeed {
        context,
        extensions: BTreeMap::new(),
    }
}

fn strip_dynamic_mcp_section(mut instructions: String) -> String {
    let head_len = {
        let head = instructions
            .split(DYNAMIC_MCP_SECTION)
            .next()
            .unwrap_or(instructions.as_str());
        head.len()
    };
    instructions.truncate(head_len);
    instructions
}

fn instruction_record(thread_id: &str, index: usize, label: &str, text: String) -> ContextRecord {
    ContextRecord {
        id: format!("instruction:{thread_id}:{index}:{label}"),
        turn_id: None,
        source: ContextSource::Instruction,
        content: vec![ContextContent::Text {
            text: Arc::from(text.as_str()),
        }],
        tool_calls: Vec::new(),
    }
}
