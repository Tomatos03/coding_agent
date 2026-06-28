use tracing_subscriber::FmtSubscriber;

use crate::{constant::NVIDIA_NEMOTRON_3_ULTRA_550B_A55B, llm::complete::chat_complete};

mod llm;
mod constant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv()?;

    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    tracing::info!("tracing initialized");

    let content = chat_complete(NVIDIA_NEMOTRON_3_ULTRA_550B_A55B, Some("You are a helpful assistant."), "Hello, how are you?").await?;

    println!("Response: {content}");

    Ok(())
}
