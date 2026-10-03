use coding_agent::agent::llm::models::{LLMClient, ToolPolicy};
use coding_agent::agent::react::history::History;
use coding_agent::bootstrap::init;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let mut history = History::new();
    history.system("你是一个编码领域的专家。")?;
    history.user("请你计算 5 * 10 / 5 的结果，只回一个数字。")?;

    let llm = LLMClient::new();
    // 没有声明工具，策略实际被忽略（请求体不会带 tool_choice）。
    let reply = llm
        .complete(history.as_slice(), None, ToolPolicy::Auto)
        .await?;

    println!("Response: {}", reply.content);
    Ok(())
}
