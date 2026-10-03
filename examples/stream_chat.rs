use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::llm::models::{LLMClient, ToolPolicy};
use coding_agent::react::history::History;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let mut history = History::new();
    history.system(SYSTEM_PROMPT)?;
    history.user("我需要去北京看故宫，怎么安排行程？")?;

    let llm = LLMClient::new();
    llm.stream(history.as_slice(), None, ToolPolicy::Auto, &mut |token| {
        print!("{token}")
    })
    .await?;

    println!();
    Ok(())
}
