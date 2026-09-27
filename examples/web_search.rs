use std::time::Duration;

use coding_agent::bootstrap;
use coding_agent::constant::search::TAVILY_API_KEY_ENV;
use coding_agent::tools::local::web_search::{SearchDepth, Topic, WebSearchArgs, WebSearchResult};

const TAVILY_SEARCH_URL: &str = "https://api.tavily.com/search";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bootstrap::init();

    let api_key = std::env::var(TAVILY_API_KEY_ENV)
        .map_err(|_| anyhow::anyhow!("缺少 {TAVILY_API_KEY_ENV}，请在 .env 中设置"))?;

    let args = WebSearchArgs {
        query: "rust async fn in trait dyn compatibility".to_string(),
        topic: Some(Topic::General),
        search_depth: Some(SearchDepth::Basic),
        max_results: Some(3),
        time_range: None,
        include_domains: None,
        exclude_domains: None,
        include_answer: Some(true),
        include_raw_content: None,
    };

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;

    let body = serde_json::to_string(&args)?;
    tracing::info!("POST {TAVILY_SEARCH_URL} body: {body}");

    let response = client
        .post(TAVILY_SEARCH_URL)
        .bearer_auth(&api_key)
        .json(&args)
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;

    if !status.is_success() {
        anyhow::bail!("Tavily 返回 {status}：{text}");
    }

    let result: WebSearchResult = serde_json::from_str(&text)?;
    tracing::info!("LLM Response: {result:#?}");

    Ok(())
}
