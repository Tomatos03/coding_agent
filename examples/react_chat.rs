//! 交互式会话 REPL：把 stdin/stdout 接到 [`Console`] 上，循环本身在 `Repl::run` 里。
//!
//! 示例只负责三件事：
//! 1. 组装 `Agent`（模型 / 工具 / system prompt / 审批策略 / 确认方）；
//! 2. 实现 [`Console`]——「怎么读、怎么显示」；
//! 3. 把 `--user` 与可选的首条提问转交进去。
//!
//! 循环逻辑（会话切换、斜杠命令、挂起提示、错误不中断）都在 `Repl::run`：
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

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;

use coding_agent::Agent;
use coding_agent::Console;
use coding_agent::Repl;
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
    /// 命令行上的位置参数：作为首条提问先喂给循环，之后再读 stdin。
    pending: VecDeque<String>,
}

#[async_trait::async_trait]
impl Console for StdinConsole {
    async fn read_line(&mut self) -> Option<String> {
        if let Some(line) = self.pending.pop_front() {
            println!("\n> {line}");
            return Some(line);
        }
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

    let mut repl = Repl::new(agent);
    let mut console = StdinConsole {
        input: BufReader::new(tokio::io::stdin()).lines(),
        pending: initial.into(),
    };
    repl.run(&mut console).await?;

    println!("再见。");
    Ok(())
}
