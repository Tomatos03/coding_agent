use async_openai::{
    error::OpenAIError,
    types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject, FunctionObjectArgs},
};
use serde_json::Value;

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn parameters(&self) -> Value;

    async fn execute(&self, args_json: &str) -> anyhow::Result<String>;

    fn definition(&self) -> anyhow::Result<ChatCompletionTools> {
        let function: FunctionObject = FunctionObjectArgs::default()
            .name(self.name())
            .description(self.description())
            .parameters(self.parameters())
            .build()
            .map_err(|e: OpenAIError| {
                anyhow::anyhow!("Failed to build tool definition for {}: {e}", self.name())
            })?;

        Ok(ChatCompletionTools::Function(ChatCompletionTool {
            function,
        }))
    }
}
