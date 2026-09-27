use async_openai::types::chat::{
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
    CreateChatCompletionRequestArgs, FinishReason, ResponseFormat, ResponseFormatJsonSchema,
};
use backon::{ExponentialBuilder, Retryable};

use crate::agent::llm::provider;
use crate::gaia::models::GaiaOutput;

pub const GAIA_PROMPT: &str = r#"You are a general AI assistant. I will ask you a question.
First, determine if you can solve this problem with your current capabilities and set "is_solvable" accordingly.
If you can solve it, set "is_solvable" to true and provide your answer in "final_answer".
If you cannot solve it, set "is_solvable" to false and explain why in "unsolvable_reason".
Your final answer should be a number OR as few words as possible OR a comma-separated list of numbers and/or strings.
If you are asked for a number, don't use a comma to write your number neither use units such as $ or percent sign unless specified otherwise.
If you are asked for a string, don't use articles, neither abbreviations (e.g., for cities), and write the digits in plain text.
If you are asked for a comma-separated list, apply the above rules depending on whether the element is a number or a string.
Respond with a single JSON object containing exactly these keys: "is_solvable" (boolean), "unsolvable_reason" (string, empty when is_solvable is true), "final_answer" (string).
"#;

pub async fn solve_gaia_question_with_retry(
    model_id: &str,
    system: &str,
    prompt: &str,
) -> anyhow::Result<GaiaOutput> {
    let op = || async { solve_gaia_question(model_id, system, prompt).await };
    op.retry(ExponentialBuilder::default().with_max_times(3))
        .await
}

pub async fn solve_gaia_question(
    model_id: &str,
    system: &str,
    prompt: &str,
) -> anyhow::Result<GaiaOutput> {
    let response_format = build_response_format()?;
    let format_setting = response_format;

    let client = async_openai::Client::with_config(provider::client_config()?);
    let request = build_request(format_setting, model_id, system, prompt)?;
    let response = client.chat().create(request).await?;

    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices returned from the model"))?;

    if choice.finish_reason == Some(FinishReason::ContentFilter) {
        return Ok(GaiaOutput {
            is_solvable: false,
            unsolvable_reason: "Model refuse to answer".to_string(),
            final_answer: String::new(),
        });
    }

    let content = choice.message.content.ok_or_else(|| {
        anyhow::anyhow!("No content returned from the model in the choice message")
    })?;

    to_gaia_output(content)
}

fn to_gaia_output(content: String) -> Result<GaiaOutput, anyhow::Error> {
    let output = serde_json::from_str::<GaiaOutput>(&content)?;
    Ok(output)
}

fn build_request(
    format_setting: ResponseFormat,
    model_id: &str,
    system: &str,
    prompt: &str,
) -> Result<async_openai::types::chat::CreateChatCompletionRequest, anyhow::Error> {
    let request = CreateChatCompletionRequestArgs::default()
        .model(model_id)
        .messages(vec![
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system)
                .build()?
                .into(),
            ChatCompletionRequestUserMessageArgs::default()
                .content(prompt)
                .build()?
                .into(),
        ])
        .response_format(format_setting)
        .build()?;
    Ok(request)
}

fn build_response_format() -> Result<ResponseFormat, anyhow::Error> {
    // DeepSeek 官方不支持 json_schema，只能退到 json_object：失去 schema 层面的
    // 约束，字段形状改由 `GAIA_PROMPT` 末尾那段 JSON 说明保证（DeepSeek 还要求
    // prompt 里出现 "json" 字样，缺了会直接 400）。
    if !provider::supports_json_schema() {
        return Ok(ResponseFormat::JsonObject);
    }

    let schema = schemars::schema_for!(GaiaOutput); // 生成 JSON Schema
    let schema_json = serde_json::to_value(&schema); // 将 JSON Schema 转换为 serde_json::Value
    let format_setting = ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: Some("The output format for the GAIA question solver.".to_string()),
            name: "gaia_output".to_string(),
            schema: schema_json?,
            strict: Some(true),
        },
    };
    Ok(format_setting)
}
