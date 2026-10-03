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
        // 唯一出口处做一次 schema 归一化：OpenAI 要求顶层是 `type: "object"`，且对
        // union type 支持不稳；而 schemars 对「内部标签枚举」只会给顶层 `oneOf`，
        // 对 `Option<T>` 会给 `type: ["X", "null"]`（见 `utils::schema`）。
        let parameters = crate::utils::schema::normalize_parameters(self.parameters());

        let function: FunctionObject = FunctionObjectArgs::default()
            .name(self.name())
            .description(self.description())
            .parameters(parameters)
            .build()
            .map_err(|e: OpenAIError| {
                anyhow::anyhow!("Failed to build tool definition for {}: {e}", self.name())
            })?;

        Ok(ChatCompletionTools::Function(ChatCompletionTool {
            function,
        }))
    }
}
