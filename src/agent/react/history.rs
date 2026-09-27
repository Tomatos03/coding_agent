use async_openai::types::chat::{
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageArgs,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
};

pub struct History {
    messages: Vec<ChatCompletionRequestMessage>,
}

impl History {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    pub fn system(&mut self, content: &str) -> anyhow::Result<()> {
        let message = ChatCompletionRequestSystemMessageArgs::default()
            .content(content)
            .build()?;
        self.messages.push(message.into());
        Ok(())
    }

    pub fn user(&mut self, content: &str) -> anyhow::Result<()> {
        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(content)
            .build()?;
        self.messages.push(message.into());
        Ok(())
    }

    pub fn assistant(
        &mut self,
        content: &str,
        tool_calls: Vec<ChatCompletionMessageToolCalls>,
    ) -> anyhow::Result<()> {
        let mut builder = ChatCompletionRequestAssistantMessageArgs::default();
        if !content.is_empty() {
            builder.content(content);
        }
        if !tool_calls.is_empty() {
            builder.tool_calls(tool_calls);
        }
        self.messages.push(builder.build()?.into());
        Ok(())
    }

    pub fn tool(&mut self, tool_call_id: &str, content: &str) -> anyhow::Result<()> {
        let message = ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(tool_call_id)
            .content(content)
            .build()?;
        self.messages.push(message.into());
        Ok(())
    }

    pub fn as_slice(&self) -> &[ChatCompletionRequestMessage] {
        &self.messages
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}
