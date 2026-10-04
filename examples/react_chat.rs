//! 交互式会话 REPL：把 stdin/stdout 接到 [`Console`] 上，循环本身在 `Agent::run` 里。
//!
//! 示例只负责三件事：
//! 1. 组装 `Agent`（模型 / 工具 / system prompt / 审批策略 / 确认方）；
//! 2. 实现 [`Console`]——「怎么读、怎么显示」；
//! 3. 把 `--user` 与可选的首条提问转交进去。
//!
//! 循环逻辑（会话切换、斜杠命令、挂起提示、错误不中断）都在 `Agent::run`：
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
use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::llm::models::LLMClient;
use coding_agent::react::approval::{Confirmer, Decision};
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
    let input: Input = Arc::new(tokio::sync::Mutex::new(
        BufReader::new(tokio::io::stdin()).lines(),
    ));

    // 交互式确认：y 批准 / n 拒绝 / s 挂起；EOF 与「没有注入 confirmer」同路径（挂起）。
    let confirmer = Confirmer::new({
        let input = input.clone();
        move |request| {
            let input = input.clone();
            async move {
                println!("\n[审批] 工具 `{}` 请求执行", request.tool);
                println!("       说明：{}", request.description);
                println!("       参数：{}", request.arguments);
                loop {
                    print!("       选择 [y] 批准 / [n] 拒绝 / [s] 稍后决定（挂起）：");
                    let _ = std::io::stdout().flush();
                    match next_line(&input).await.as_deref().map(str::trim) {
                        Some("y" | "Y") => return Decision::Approve,
                        Some("n" | "N") => return Decision::Deny,
                        Some("s" | "S") => return Decision::Pending,
                        // EOF：无人可答 → 挂起（与「没有注入 confirmer」同一条路径）。
                        None => return Decision::Pending,
                        _ => println!("       请输入 y / n / s"),
                    }
                }
            }
        }
    });

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

    let mut console = StdinConsole {
        input,
        pending: initial.into(),
    };
    agent.run(&mut console).await?;

    println!("再见。");
    Ok(())
}
