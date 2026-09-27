use coding_agent::agent::llm::models::{Completer, LLMClient};
use coding_agent::agent::react::history::History;
use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let mut history = History::new();
    history.system(SYSTEM_PROMPT)?;
    history.user("我需要去北京看故宫，怎么安排行程？")?;

    let client = LLMClient::new();
    client
        .stream(history.as_slice(), None, &mut |token| print!("{token}"))
        .await?;

    println!();
    Ok(())
}
