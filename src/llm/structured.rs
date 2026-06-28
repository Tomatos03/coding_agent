use async_openai::{
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ResponseFormat, ResponseFormatJsonSchema,
    },
};

use crate::models::action_plan::ActionPlan;

pub async fn chat_complete_structured(
    model: &str,
    system: Option<&str>,
    prompt: &str,
) -> anyhow::Result<ActionPlan> {
    let config = OpenAIConfig::new()
        .with_api_base(std::env::var("OPENAI_API_BASE_URL")?)
        .with_api_key(std::env::var("OPENAI_API_KEY")?);

    let client = async_openai::Client::with_config(config);
    let mut messages = vec![];
    if let Some(system) = system {
        messages.push(
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system)
                .build()?
                .into(),
        );
    }
    messages.push(
        ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()?
            .into(),
    );

    let schema = schemars::schema_for!(ActionPlan);
    let schema_json = schema.as_value().clone();
    let format_setting = ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: Some(
                "A step-by-step agent action plan with difficulty and time estimate".into(),
            ),
            name: "action_plan".into(),
            schema: schema_json,
            strict: Some(true),
        },
    };

    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .response_format(format_setting)
        .messages(messages)
        .max_tokens(2048u32)
        .build()?;

    let response = client.chat().create(request).await?;

    tracing::info!("LLM Response: {:#?}", response);

    let plan = response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .ok_or_else(|| anyhow::anyhow!("No content in response"))
        .and_then(|content| {
            serde_json::from_str::<ActionPlan>(&content)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize response: {}", e))
        })?;

    Ok(plan)
}
