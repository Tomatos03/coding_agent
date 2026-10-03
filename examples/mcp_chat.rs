//! 真实 LLM + MCP：从 `mcp.json` 加载 MCP server，交给真实模型自主决定是否调用。
//!
//! 与 `react_chat` 的区别：这里显式区分并打印「本地工具 / MCP 工具」，
//! 并在没有加载到 MCP 工具时给出提示。审批策略从 `.agents/settings.json` 读取
//! （缺文件 = 全放行），凡是策略判 `ask` 的调用都会先在这里的终端上征求确认。
//!
//! 前置：
//!   1. `cp mcp.example.json mcp.json` 并填入真实 server；
//!   2. `.env` 配好 provider 与模型（见 `.claude/rules/commands.md`）；
//!   3. 可选：`cp .agents/settings.example.json .agents/settings.json` 启用确认。
//!
//! 运行（会真实调用 LLM，产生 API 消耗）：
//!   cargo run --example mcp_chat
//!   cargo run --example mcp_chat -- "用 MCP 的 echo 工具确认链路"

use std::io::Write as _;
use std::sync::Arc;

use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::llm::models::LLMClient;
use coding_agent::react::approval::{ApprovalRequest, Confirmer, Decision};
use coding_agent::react::models::{DEFAULT_MAX_TURNS, Step};
use coding_agent::react::runner::ReactLoop;
use coding_agent::settings::{ApprovalAction, SETTINGS_PATH, load_settings};
use coding_agent::tools::build_tools;

/// 交互式确认方：打印请求详情，读一行 stdin；`y` / `yes`（不分大小写）才批准。
struct StdinConfirmer;

#[async_trait::async_trait]
impl Confirmer for StdinConfirmer {
    async fn confirm(&self, request: &ApprovalRequest) -> Decision {
        println!();
        println!("[确认] 模型请求执行工具 `{}`", request.tool);
        if !request.description.is_empty() {
            println!("       说明：{}", request.description);
        }
        println!("       参数：{}", request.arguments);
        print!("       允许执行？[y/N] ");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(_) if is_yes(line.trim()) => Decision::Approve,
            _ => Decision::Deny,
        }
    }
}

fn is_yes(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

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

    if !std::path::Path::new(SETTINGS_PATH).exists() {
        eprintln!(
            "提示：当前目录下没有 {SETTINGS_PATH}，所有工具默认放行。\
             可先执行 `cp .agents/settings.example.json .agents/settings.json` 启用确认。"
        );
    }

    // 从当前目录的 mcp.json 加载「本地 + MCP」工具，再从 .agents/settings.json 加载审批策略。
    let settings = load_settings()?;
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

    let mut require_confirmation: Vec<&str> = tools
        .keys()
        .map(String::as_str)
        .filter(|name| settings.approval.action_for(name) == ApprovalAction::Ask)
        .collect();
    require_confirmation.sort_unstable();
    if require_confirmation.is_empty() {
        println!("审批策略：全部放行（{SETTINGS_PATH} 未配置 ask 规则）");
    } else {
        println!("审批策略：以下工具执行前需要确认 {require_confirmation:?}");
    }
    println!();

    // 真实 LLM：需要 .env 中的 provider 与模型配置。
    let mut agent = ReactLoop::new(
        Arc::new(LLMClient::new()),
        tools,
        SYSTEM_PROMPT,
        DEFAULT_MAX_TURNS,
    )?
    .with_approval_policy(settings.approval)
    .with_confirmer(Arc::new(StdinConfirmer));

    println!("User: {prompt}\n");

    let outcome = agent
        .run(
            &prompt,
            &mut |step| match step {
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
            },
            &mut |_, _| {},
        )
        .await?;

    println!(
        "\n---终止于 {:?}，共 {} 轮 ---",
        outcome.termination, outcome.turns
    );

    Ok(())
}
