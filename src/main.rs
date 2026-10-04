//! 项目入口：组装并启动交互式 Agent（stdin/stdout REPL）。
//!
//! 循环骨架在库里的 `Repl`（读取 → 评估 → 输出），交互协议（斜杠命令、审批挂起
//! 与恢复）在内置的 `AgentEvaluator` 里，这里只负责接线：
//!
//! 1. 组装 `Agent`：模型（`LLMClient`）、工具表（本地 + MCP）、system prompt、
//!    审批策略（`.agents/settings.json`）与交互式确认方；
//! 2. 用库自带的终端适配（`StdinReader` / `StdoutWriter`）接 stdin/stdout；
//! 3. 跑 `Repl::new(reader, evaluator, writer).run()`，直到 EOF / `/quit`。
//!
//! 运行（需要 LLM 凭证）：
//!
//! ```bash
//! cargo run
//! ```
//!
//! 会话是内存实现，退出即丢失全部会话。

use std::sync::Arc;

use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::llm::models::LLMClient;
use coding_agent::react::approval::TerminalConfirmer;
use coding_agent::react::models::DEFAULT_MAX_TURNS;
use coding_agent::settings::load_settings;
use coding_agent::tools::build_tools;
use coding_agent::{Agent, AgentEvaluator, Repl, StdinReader, StdoutWriter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let settings = load_settings()?;

    // 交互式确认：仅 y 批准 / n 拒绝；未做出选择则一直等待，EOF 挂起。
    let confirmer = TerminalConfirmer::new();

    let agent = Agent::builder(
        Arc::new(LLMClient::new()),
        build_tools().await?,
        SYSTEM_PROMPT,
        DEFAULT_MAX_TURNS,
    )
    .approval_policy(settings.approval)
    .confirmer(Arc::new(confirmer))
    .in_memory();

    let evaluator = AgentEvaluator::new(agent);

    println!("agent running... 输入 /help 查看命令，/quit 退出。");

    let reader = StdinReader::new();
    let writer = StdoutWriter;
    let mut repl = Repl::new(reader, evaluator, writer);
    repl.run().await?;

    println!("再见。");
    Ok(())
}
