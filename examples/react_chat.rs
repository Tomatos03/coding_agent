//! 交互式会话 REPL：用库自带的 `StdinReader` / `StdoutWriter` 接 stdin/stdout，
//! 循环骨架在库里的 `Repl`（读取 → 评估 → 输出），交互协议（斜杠命令、挂起恢复）
//! 在内置的 `AgentEvaluator` 里。
//!
//! 示例只负责三件事：
//! 1. 组装 `Agent`（模型 / 工具 / system prompt / 审批策略 / 确认方）；
//! 2. 用库自带的终端适配（`StdinReader` / `StdoutWriter`）；
//! 3. 把 `--user` 与可选的首条提问转交进去。
//!
//! `AgentEvaluator` 负责会话切换、斜杠命令、挂起提示与错误不中断：
//!
//! - 输入普通文字 = 对当前会话追问（同一份历史，模型能看到此前所有轮次）；
//! - 审批闸门判 `ask` 时，你可以 `y` 批准、`n` 拒绝，或 `s` **挂起**——
//!   会话停在未执行完的工具批次上，随时用 `/resume` 回来批；
//! - 斜杠命令管理多个会话（`/sessions`、`/switch`、`/delete`、`/new`）。
//!
//! 注意：本轮是内存实现，**退出即丢失全部会话**；跨重启保留要等文件后端。
//!
//! 运行（需要 LLM 凭证；想看到审批挂起，请在 `.agents/settings.json` 里给某个工具配 `ask`）：
//!   cargo run --example react_chat
//!   cargo run --example react_chat -- --user alice "先读一下 README"

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

    // 可选 `--user <id>`；其余位置参数按顺序作为首批提问。
    let mut user_id = None;
    let mut initial: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--user" => user_id = args.next(),
            _ => initial.push(arg),
        }
    }

    let settings = load_settings()?;

    // 交互式确认：仅 y 批准 / n 拒绝；未做出选择则一直等待，EOF 挂起。
    let confirmer = TerminalConfirmer::new();

    let mut builder = Agent::builder(
        Arc::new(LLMClient::new()),
        build_tools().await?,
        SYSTEM_PROMPT,
        DEFAULT_MAX_TURNS,
    )
    .approval_policy(settings.approval)
    .confirmer(Arc::new(confirmer));
    let user_label = user_id.clone();
    if let Some(user) = user_id {
        builder = builder.default_user(user);
    }
    let agent = builder.in_memory();

    if let Some(user) = user_label {
        println!("当前 user_id：{user}");
    }

    let evaluator = AgentEvaluator::new(agent);
    let reader = StdinReader::with_pending(initial);
    let writer = StdoutWriter;
    let mut repl = Repl::new(reader, evaluator, writer);
    repl.run().await?;

    println!("再见。");
    Ok(())
}
