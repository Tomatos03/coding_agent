use async_openai::config::OpenAIConfig;
use std::sync::OnceLock;

use crate::constant::provider::{
    DEFAULT_MAX_TOKENS, DEFAULT_PROVIDER, MAX_TOKENS_ENV, MODEL_ID_ENV, OPENROUTER,
    PROVIDER_BASE_URL_VARS, PROVIDER_ENV,
};

fn current_provider() -> &'static str {
    static PROVIDER: OnceLock<String> = OnceLock::new();
    PROVIDER.get_or_init(|| {
        std::env::var(PROVIDER_ENV)
            .map(|p| p.trim().to_lowercase())
            .unwrap_or_else(|_| DEFAULT_PROVIDER.to_string())
    })
}

pub fn model_id() -> anyhow::Result<String> {
    std::env::var(MODEL_ID_ENV).map_err(|_| {
        anyhow::anyhow!(
            "缺少环境变量 {}，请填写当前 provider 对应的模型 ID",
            MODEL_ID_ENV
        )
    })
}

pub fn max_tokens() -> u32 {
    std::env::var(MAX_TOKENS_ENV)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

pub fn client_config() -> anyhow::Result<OpenAIConfig> {
    let provider = current_provider();

    let Some((_, base_url_var, api_key_var)) = PROVIDER_BASE_URL_VARS
        .iter()
        .find(|(name, _, _)| *name == provider)
    else {
        return Err(anyhow::anyhow!(
            "未知的 provider: {provider}（可选：{}）",
            PROVIDER_BASE_URL_VARS
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    };

    let read = |var: &str| {
        std::env::var(var).map_err(|_| anyhow::anyhow!("provider={provider} 缺少环境变量 {var}"))
    };

    Ok(OpenAIConfig::new()
        .with_api_base(read(base_url_var)?)
        .with_api_key(read(api_key_var)?))
}

pub fn supports_json_schema() -> bool {
    current_provider() == OPENROUTER
}

pub fn schema_hint(schema: &serde_json::Value) -> String {
    format!(
        "Respond with a single JSON object and nothing else, \
         conforming to this JSON Schema:\n{schema}"
    )
}
