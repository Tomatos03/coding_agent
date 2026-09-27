//! 真实 LLM + MCP：从 `mcp.json` 加载 MCP server，交给真实模型自主决定是否调用。
//!
//! 与 `react_chat` 的区别：这里显式区分并打印「本地工具 / MCP 工具」，
//! 并在没有加载到 MCP 工具时给出提示。
//!
//! 前置：
//!   1. `cp mcp.example.json mcp.json` 并填入真实 server；
//!   2. `.env` 配好 provider 与模型（见 `.claude/rules/commands.md`）。
//!
//! 运行（会真实调用 LLM，产生 API 消耗）：
//!   cargo run --example mcp_chat
//!   cargo run --example mcp_chat -- "用 MCP 的 echo 工具确认链路"

use std::sync::Arc;

use coding_agent::agent::llm::models::LLMClient;
use coding_agent::agent::react::models::{DEFAULT_MAX_TURNS, Step};
use coding_agent::agent::react::runner::ReactLoop;
use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::tools::build_tools;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let prompt = std::env::args().nth(1).unwrap_or_else(|| {
        "请调用一个 MCP 工具来确认链路是否可用，并告诉我它返回了什么。".to_owned()
    });

    if !std::path::Path::new("mcp.json").exists() {
        eprintln!(
            "提示：当前目录 {} 下没有 mcp.json，只会加载本地工具。\
             可先执行 `cp mcp.example.json mcp.json` 并配置 server。",
            std::env::current_dir()?.display()
        );
    }

    // 从当前目录的 mcp.json 加载「本地 + MCP」工具。
    let tools = build_tools().await?;

    let mut local_tools: Vec<&str> = Vec::new();
    let mut mcp_tools: Vec<&str> = Vec::new();
    for name in tools.keys() {
        // MCP 工具的暴露名形如 `{server}__{tool}`。
        if name.contains("__") {
            mcp_tools.push(name.as_str());
        } else {
            local_tools.push(name.as_str());
        }
    }
    local_tools.sort_unstable();
    mcp_tools.sort_unstable();

    println!("本地工具：{local_tools:?}");
    println!("MCP 工具：{mcp_tools:?}");
    if mcp_tools.is_empty() {
        eprintln!("警告：没有加载到任何 MCP 工具，本次对话只会使用本地工具。");
    }
    println!();

    // 真实 LLM：需要 .env 中的 provider 与模型配置。
    let mut agent = ReactLoop::new(
        Arc::new(LLMClient::new()),
        tools,
        SYSTEM_PROMPT,
        DEFAULT_MAX_TURNS,
    )?;

    println!("User: {prompt}\n");

    let outcome = agent
        .run(
            &prompt,
            |step| match step {
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
                Step::Nudge { turn, reason } => println!("[{turn}] 重试：{reason}"),
            },
            |_, _| {},
        )
        .await?;

    println!(
        "\n---终止于 {:?}，共 {} 轮 ---",
        outcome.termination, outcome.turns
    );

    Ok(())
}
