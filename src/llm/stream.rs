use async_openai::{
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs,
    },
};
use async_stream::stream;
use futures::{Stream, StreamExt};

pub async fn chat_stream(
    model: &str,
    system: Option<&str>,
    prompt: &str,
) -> impl Stream<Item = anyhow::Result<String>> {
    stream! {
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

        let request = CreateChatCompletionRequestArgs::default()
            .model(model)
            .messages(messages)
            .max_tokens(2048u32)
            .build()?;

        let mut stream = client
            .chat()
            .create_stream(request)
            .await?;

        while let Some(response_result) = stream.next().await {
            match response_result {
                Ok(chunk) => {
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(content) = &choice.delta.content {
                            yield Ok(content.clone());
                        }
                    }
                }
                Err(err) => yield Err(err.into())
            }
        }
    }
}
