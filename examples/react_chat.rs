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

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "什么是MCP?".to_owned());

    let mut agent = ReactLoop::new(
        Arc::new(LLMClient::new()),
        build_tools().await?,
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
