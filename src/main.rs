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
use coding_agent::react::approval::Confirmer;
use coding_agent::react::models::{DEFAULT_MAX_TURNS, Step};
use coding_agent::settings::load_settings;
use coding_agent::tools::build_tools;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

/// 共享的 stdin 行读取器：REPL 的 `Console` 与确认方都用它，靠 `Mutex` 串行化。
type Input = Arc<tokio::sync::Mutex<Lines<BufReader<Stdin>>>>;

async fn next_line(input: &Input) -> Option<String> {
    input.lock().await.next_line().await.ok().flatten()
}

/// 把 stdin/stdout 接到 [`Console`]：库里决定「什么时候读」，这里只决定「怎么读、怎么显示」。
struct StdinConsole {
    input: Input,
}

#[async_trait::async_trait]
impl Console for StdinConsole {
    async fn read_line(&mut self) -> Option<String> {
        print!("\n> ");
        let _ = std::io::stdout().flush();
        next_line(&self.input).await
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
    let input: Input = Arc::new(tokio::sync::Mutex::new(
        BufReader::new(tokio::io::stdin()).lines(),
    ));

    // 交互式确认：仅 y 批准 / n 拒绝；未做出选择则一直等待，EOF 挂起。
    let confirmer = Confirmer::interactive({
        let input = input.clone();
        move || {
            let input = input.clone();
            async move { next_line(&input).await }
        }
    });

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

    let mut console = StdinConsole { input };
    agent.run(&mut console).await?;

    println!("再见。");
    Ok(())
}
