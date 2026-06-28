use coding_agent::{constant::NVIDIA_NEMOTRON_3_ULTRA_550B_A55B, llm::structured::{chat_complete_structured}};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv()?;

    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    tracing::info!("tracing initialized");

    let content = chat_complete_structured(
        NVIDIA_NEMOTRON_3_ULTRA_550B_A55B,
        Some("你是一个全能助手"),
        "我需要去北京看故宫，怎么安排行程？",
    )
    .await?;

    println!("Response: {content:#?}");
    Ok(())
}
