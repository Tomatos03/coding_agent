use coding_agent::{
    bootstrap::init,
    tools::{ToolHashMap, build_tools},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rust async fn in trait dyn compatibility".to_owned());

    let registry: ToolHashMap = build_tools().await?;

    let mut names: Vec<&str> = registry.keys().map(String::as_str).collect();
    names.sort_unstable();
    println!("已注册工具：{}", names.join(", "));

    let tool = registry
        .get("web_search")
        .ok_or_else(|| anyhow::anyhow!("注册表里没有 web_search"))?;

    let args_json = serde_json::json!({ "query": query, "max_results": 3u32 }).to_string();
    println!("调用 web_search，参数：{args_json}");

    let output = tool.execute(&args_json).await?;

    println!("{output}");
    Ok(())
}
