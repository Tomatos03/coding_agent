use coding_agent::agent::llm::models::{Completer, LLMClient};
use coding_agent::agent::react::history::History;
use coding_agent::bootstrap::init;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let mut history = History::new();
    history.system("你是一个编码领域的专家。")?;
    history.user("请你计算 5 * 10 / 5 的结果，只回一个数字。")?;

    let client = LLMClient::new();
    let reply = client.complete(history.as_slice(), None).await?;

    println!("Response: {}", reply.content);
    Ok(())
}
