//! 端到端冒烟测试：需要真实 LLM 配置（GASKET_LLM_*），默认 #[ignore]。
//!
//! 运行方式（需 .env 或环境变量已配置）：
//! ```sh
//! set -a && source .env && set +a
//! cargo test -p gasket-host --test smoke_llm -- --ignored --nocapture
//! ```
//!
//! 等价于 plan Task 6 Step 3 的冒烟测试 2（基础对话）+ 3（工具调用），
//! 只是不经过 reedline/TTY，可在 CI 或脚本里重复运行。

#![cfg(test)]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gasket_core::{
    built_in_tools, run_agent_loop, AgentContext, AgentMessage, ContentBlock, UserMessage,
};
use gasket_host::{ConfigLoader, EventPrinter, Mode, PermissionPolicy};

/// 构造一次最小的 agent loop 调用，验证 ConfigLoader + run_agent_loop + EventPrinter
/// 端到端能跑通（真实 LLM）。
#[tokio::test]
#[ignore]
async fn end_to_end_basic_chat() {
    let cfg = ConfigLoader::load().expect("GASKET_LLM_* must be set");

    // FullAuto 模式，避免 approver 阻塞（无 stdin）。
    let config = cfg.build_loop_config(
        3,
        Some(Arc::new(AtomicBool::new(false))),
        Some(Arc::new(PermissionPolicy::new(Mode::FullAuto, |_, _| false))),
    );

    let user_msg = AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text("Reply with exactly: pong")],
        timestamp: gasket_core::now(),
    });
    let cwd = std::env::current_dir().unwrap();
    let context = AgentContext {
        system_prompt: "You are a test assistant. Follow instructions exactly.".into(),
        messages: vec![],
        tools: built_in_tools(),
        cwd,
        env: std::env::vars().collect(),
        session_id: "smoke-test".into(),
    };

    let mut buf: Vec<u8> = Vec::new();
    let new_msgs = run_agent_loop(vec![user_msg], context, config, |ev| {
        let mut p = EventPrinter::new(&mut buf);
        p.on_event(&ev);
    })
    .await
    .expect("agent loop should complete");

    // 至少有一条 assistant 消息。
    assert!(
        new_msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::Assistant(_))),
        "expected at least one assistant message, got: {:?}",
        new_msgs
    );

    // EventPrinter 应该输出了点什么（至少 usage 或文本）。
    let out = String::from_utf8_lossy(&buf);
    assert!(!out.is_empty(), "printer produced no output");
    eprintln!("--- printer output ---\n{out}");
}

/// 冒烟测试 3：工具调用。FullAuto 模式下让 LLM 调用 `list` 工具。
#[tokio::test]
#[ignore]
async fn end_to_end_tool_call() {
    let cfg = ConfigLoader::load().expect("GASKET_LLM_* must be set");

    let config = cfg.build_loop_config(
        3,
        Some(Arc::new(AtomicBool::new(false))),
        Some(Arc::new(PermissionPolicy::new(Mode::FullAuto, |_, _| false))),
    );

    let user_msg = AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(
            "Use the `list` tool to list the current directory, then reply with the count of entries in one sentence.",
        )],
        timestamp: gasket_core::now(),
    });
    let cwd = std::env::current_dir().unwrap();
    let context = AgentContext {
        system_prompt: "You are a test assistant. You MUST use tools when asked to.".into(),
        messages: vec![],
        tools: built_in_tools(),
        cwd: cwd.clone(),
        env: std::env::vars().collect(),
        session_id: "smoke-test-tool".into(),
    };

    let new_msgs = run_agent_loop(vec![user_msg], context, config, |_| {})
        .await
        .expect("agent loop should complete");

    // FullAuto 模式下，list 是 Low risk，应该被允许执行 -> 产生 ToolResult。
    let has_tool_result = new_msgs
        .iter()
        .any(|m| matches!(m, AgentMessage::ToolResult(_)));
    assert!(
        has_tool_result,
        "expected at least one ToolResult message (FullAuto should allow `list`), got: {:?}",
        new_msgs
            .iter()
            .map(|m| match m {
                AgentMessage::User(_) => "User",
                AgentMessage::Assistant(_) => "Assistant",
                AgentMessage::ToolResult(t) => t.tool_name.as_str(),
                AgentMessage::Custom(_) => "Custom",
            })
            .collect::<Vec<_>>()
    );
    eprintln!("tool call flow OK; messages: {}", new_msgs.len());
}
