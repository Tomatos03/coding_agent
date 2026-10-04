//! 项目入口：组装并启动交互式 Agent（stdin/stdout REPL）。
//!
//! 循环逻辑（多轮会话、斜杠命令、审批挂起与恢复）都在库里的 `Agent::run`，
//! 这里只负责接线：
//!
//! 1. 组装 `Agent`：模型（`LLMClient`）、工具表（本地 + MCP）、system prompt、
//!    审批策略（`.agents/settings.json`）与交互式确认方；
//! 2. 实现 [`Console`]：决定「怎么读 stdin、怎么把 `Step` 显示到 stdout」；
//! 3. 跑 `agent.run(&mut console)`，直到 EOF / `/quit`。
//!
//! 运行（需要 LLM 凭证）：
//!
//! ```bash
//! cargo run
//! ```
//!
//! 会话是内存实现，退出即丢失全部会话。

use std::io::Write;
use std::sync::Arc;

use coding_agent::Agent;
use coding_agent::Console;
use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::llm::models::LLMClient;
use coding_agent::react::approval::TerminalConfirmer;
use coding_agent::react::models::{DEFAULT_MAX_TURNS, Step};
use coding_agent::settings::load_settings;
use coding_agent::tools::build_tools;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

/// 把 stdin/stdout 接到 [`Console`]：库里决定「什么时候读」，这里只决定「怎么读、怎么显示」。
/// 确认方（`TerminalConfirmer`）直接读 stdin，不经过这里。
struct StdinConsole {
    input: Lines<BufReader<Stdin>>,
}

#[async_trait::async_trait]
impl Console for StdinConsole {
    async fn read_line(&mut self) -> Option<String> {
        print!("\n> ");
        let _ = std::io::stdout().flush();
        self.input.next_line().await.ok().flatten()
    }

    fn print(&mut self, line: &str) {
        println!("{line}");
    }

    fn step(&mut self, step: &Step) {
        match step {
            Step::Thought { turn, content } => println!("[{turn}] 思考：{content}"),
            Step::Answer { turn, content } => println!("\n[{turn}] 答案：{content}"),
            Step::Action {
                turn,
                name,
                arguments,
            } => println!("[{turn}] 调用：{name} 参数 {arguments}"),
            Step::Observation { turn, name, output } => {
                println!("[{turn}] {name} 返回：{output}");
            }
        }
    }
}

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

    println!("agent running... 输入 /help 查看命令，/quit 退出。");

    let mut console = StdinConsole {
        input: BufReader::new(tokio::io::stdin()).lines(),
    };
    agent.run(&mut console).await?;

    println!("再见。");
    Ok(())
}
