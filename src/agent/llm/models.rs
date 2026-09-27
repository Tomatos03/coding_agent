use super::provider;
use crate::constant::provider::MAX_TOKENS_ENV;

use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestMessage, ChatCompletionTools,
    CreateChatCompletionRequestArgs, FinishReason, FunctionCall,
};
use futures::StreamExt;

#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<ChatCompletionMessageToolCalls>,
}

#[async_trait::async_trait]
pub trait Completer: Send + Sync {
    async fn complete(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&[ChatCompletionTools]>,
    ) -> anyhow::Result<Reply>;

    async fn stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&[ChatCompletionTools]>,
        on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> anyhow::Result<Reply>;
}

pub struct LLMClient {
    model: String,
    max_tokens: u32,
    internal_client: async_openai::Client<OpenAIConfig>,
}

impl LLMClient {
    pub fn new() -> Self {
        let model = provider::model_id().expect("缺少 CURRENT_USE_MODEL_ID，请检查 .env");
        let internal_client = async_openai::Client::with_config(
            provider::client_config().expect("缺少 provider 配置，请检查 .env"),
        );

        Self {
            model,
            max_tokens: provider::max_tokens(),
            internal_client,
        }
    }

    fn request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&[ChatCompletionTools]>,
    ) -> anyhow::Result<async_openai::types::chat::CreateChatCompletionRequest> {
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder
            .model(&self.model)
            .messages(messages.to_vec())
            .max_tokens(self.max_tokens);
        if let Some(tools) = tools {
            builder.tools(tools.to_vec());
        }
        Ok(builder.build()?)
    }
}

#[async_trait::async_trait]
impl Completer for LLMClient {
    async fn complete(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&[ChatCompletionTools]>,
    ) -> anyhow::Result<Reply> {
        let request = self.request(messages, tools)?;
        let response = self.internal_client.chat().create(request).await?;
        tracing::info!("LLM Response: {:#?}", response);

        let message = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No choices in response"))?
            .message;

        Ok(Reply {
            content: message.content.unwrap_or_default(),
            tool_calls: message.tool_calls.unwrap_or_default(),
        })
    }

    async fn stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&[ChatCompletionTools]>,
        on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> anyhow::Result<Reply> {
        let request = self.request(messages, tools)?;

        let mut content = String::new();
        let mut accumulator = ToolCallAccumulator::default();
        let mut finish_reason = None;

        let mut stream = self.internal_client.chat().create_stream(request).await?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;

            let Some(choice) = chunk.choices.first() else {
                continue;
            };

            if let Some(reason) = &choice.finish_reason {
                finish_reason = Some(*reason);
            }

            if let Some(delta) = &choice.delta.content {
                content.push_str(delta);
                on_token(delta);
            }

            for fragment in choice.delta.tool_calls.iter().flatten() {
                accumulator.absorb(fragment);
            }
        }

        let tool_calls = accumulator.finish();

        if content.is_empty()
            && tool_calls.is_empty()
            && finish_reason == Some(FinishReason::Length)
        {
            tracing::warn!(
                "预算（{} token）耗尽却零产出：推理模型的 reasoning 分片会被 async-openai 丢弃，实际消耗不可见。可调大 {MAX_TOKENS_ENV}",
                self.max_tokens
            );
        }

        Ok(Reply {
            content,
            tool_calls,
        })
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    slots: Vec<ToolCallFragment>,
}

impl ToolCallAccumulator {
    fn absorb(&mut self, chunk: &ChatCompletionMessageToolCallChunk) {
        let index = chunk.index as usize;
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, ToolCallFragment::default);
        }
        self.slots[index].absorb(chunk);
    }

    fn finish(self) -> Vec<ChatCompletionMessageToolCalls> {
        self.slots
            .into_iter()
            .filter(|fragment| !fragment.name.is_empty())
            .map(|fragment| {
                ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                    id: fragment.id,
                    function: FunctionCall {
                        name: fragment.name,
                        arguments: fragment.arguments,
                    },
                })
            })
            .collect()
    }
}

#[derive(Default)]
struct ToolCallFragment {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallFragment {
    fn absorb(&mut self, fragment: &async_openai::types::chat::ChatCompletionMessageToolCallChunk) {
        if let Some(id) = &fragment.id {
            self.id.push_str(id);
        }
        if let Some(function) = &fragment.function {
            if let Some(name) = &function.name {
                self.name.push_str(name);
            }
            if let Some(arguments) = &function.arguments {
                self.arguments.push_str(arguments);
            }
        }
    }
}

impl Default for LLMClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::chat::{ChatCompletionMessageToolCallChunk, FunctionCallStream};

    fn fragment(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> ChatCompletionMessageToolCallChunk {
        ChatCompletionMessageToolCallChunk {
            index,
            id: id.map(str::to_owned),
            r#type: None,
            function: Some(FunctionCallStream {
                name: name.map(str::to_owned),
                arguments: arguments.map(str::to_owned),
            }),
        }
    }

    fn names_and_arguments(calls: &[ChatCompletionMessageToolCalls]) -> Vec<(String, String)> {
        calls
            .iter()
            .map(|call| match call {
                ChatCompletionMessageToolCalls::Function(f) => {
                    (f.function.name.clone(), f.function.arguments.clone())
                }
                other => panic!("只应有 function 调用，实际是 {other:?}"),
            })
            .collect()
    }

    #[test]
    fn reassembles_one_call_split_across_chunks() {
        let mut accumulator = ToolCallAccumulator::default();

        accumulator.absorb(&fragment(0, Some("call_1"), Some("echo"), None));
        accumulator.absorb(&fragment(0, None, None, Some(r#"{"q""#)));
        accumulator.absorb(&fragment(0, None, None, Some(r#":"#)));
        accumulator.absorb(&fragment(0, None, None, Some(r#""hi"}"#)));

        assert_eq!(
            names_and_arguments(&accumulator.finish()),
            vec![("echo".to_owned(), r#"{"q":"hi"}"#.to_owned())]
        );
    }

    #[test]
    fn keeps_interleaved_calls_apart() {
        let mut accumulator = ToolCallAccumulator::default();

        accumulator.absorb(&fragment(0, Some("a"), Some("echo"), Some("{}")));
        accumulator.absorb(&fragment(1, Some("b"), Some("nope"), Some("{}")));
        accumulator.absorb(&fragment(0, None, None, Some("tail")));

        assert_eq!(
            names_and_arguments(&accumulator.finish()),
            vec![
                ("echo".to_owned(), "{}tail".to_owned()),
                ("nope".to_owned(), "{}".to_owned()),
            ]
        );
    }

    #[test]
    fn drops_slots_that_never_got_a_name() {
        let mut accumulator = ToolCallAccumulator::default();

        accumulator.absorb(&fragment(0, Some("a"), Some("echo"), Some("{}")));
        accumulator.absorb(&fragment(2, Some("c"), Some("nope"), Some("{}")));

        assert_eq!(
            names_and_arguments(&accumulator.finish()),
            vec![
                ("echo".to_owned(), "{}".to_owned()),
                ("nope".to_owned(), "{}".to_owned()),
            ]
        );
    }
}
