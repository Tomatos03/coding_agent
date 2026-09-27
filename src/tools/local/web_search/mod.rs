//! Tavily 网页搜索工具。

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::constant::search::TAVILY_API_KEY_ENV;
use crate::tools::tool::Tool;

pub struct WebSearch;

#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct WebSearchArgs {
    #[schemars(description = "搜索查询语句。")]
    pub query: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "搜索类别。general 通用；news 实时新闻；finance 财经。默认 general。"
    )]
    pub topic: Option<Topic>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "检索深度，权衡延迟与相关性。basic 均衡（1 credit）；advanced 最相关但更慢（2 credits）；fast 低延迟；ultra-fast 最低延迟。默认 basic。"
    )]
    pub search_depth: Option<SearchDepth>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        range(max = 20),
        description = "返回结果条数上限，取值 0-20。默认 10。"
    )]
    pub max_results: Option<u8>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "按发布/更新时间回溯的时间窗：day/week/month/year。默认不限制。")]
    pub time_range: Option<TimeRange>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "限定只在这些域名内搜索，最多 300 个。")]
    pub include_domains: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "排除这些域名，最多 150 个。")]
    pub exclude_domains: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "是否附带一段由 LLM 生成的答案摘要。默认 false。")]
    pub include_answer: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "是否附带每条结果的原始正文（Markdown）。内容较长，仅在需要细节时开启。默认 false。"
    )]
    pub include_raw_content: Option<bool>,
}

#[derive(Debug, Clone, Copy, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum Topic {
    General,
    News,
    Finance,
}

#[derive(Debug, Clone, Copy, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[schemars(inline)]
pub enum SearchDepth {
    Basic,
    Advanced,
    Fast,
    UltraFast,
}

#[derive(Debug, Clone, Copy, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum TimeRange {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebSearchResult {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    pub results: Vec<WebSearchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchItem {
    pub title: String,
    pub url: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_content: Option<String>,
}

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Performs a web search"
    }

    fn parameters(&self) -> Value {
        // `WebSearchArgs` 的 schema 是静态的，序列化失败属于编程错误。
        serde_json::to_value(schema_for!(WebSearchArgs))
            .expect("failed to serialize WebSearchArgs schema")
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: WebSearchArgs = serde_json::from_str(args_json).map_err(|e| {
            anyhow::anyhow!("[{}] Failed to deserialize arguments: {e}", self.name())
        })?;
        let api_key = std::env::var(TAVILY_API_KEY_ENV)
            .map_err(|_| anyhow::anyhow!("缺少环境变量 {TAVILY_API_KEY_ENV}，请在 .env 中设置"))?;
        let client = reqwest::Client::new();
        let response = client
            .post("https://api.tavily.com/search")
            .bearer_auth(api_key)
            .json(&args)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            anyhow::bail!("web_search_tool call failed: {status} {text}");
        }
        let result = serde_json::from_str::<WebSearchResult>(&text);
        match result {
            Err(e) => Ok(format!("Error: {}", e)),
            std::result::Result::Ok(val) => Ok(serde_json::to_string(&val)?),
        }
    }
}
