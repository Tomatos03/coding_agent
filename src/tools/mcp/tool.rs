use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ResourceContents,
    Tool as RemoteTool,
};
use serde_json::{Map, Value};

use crate::tools::mcp::connection::McpConnection;
use crate::tools::tool::Tool;

/// function 名允许的最大长度。
const MAX_FUNCTION_NAME_LEN: usize = 64;

/// 拼装暴露名时 server 与工具名之间的分隔符。
const NAME_SEPARATOR: &str = "__";

/// 把远端 MCP 工具适配成本项目的 `Tool`。
pub struct McpTool {
    connection: Arc<McpConnection>,
    remote_name: String,
    exposed_name: String,
    description: String,
    schema: Value,
}

impl McpTool {
    /// 由连接与远端工具构造；名字非法或超长时返回 `Err`。
    ///
    /// 名字必须合法：`Tool::definition()` 最终会交给 OpenAI function 名校验
    /// （`^[a-zA-Z0-9_-]{1,64}$`），一个非法名会让整个 `ReactLoop` 构造失败。
    pub fn try_new(connection: Arc<McpConnection>, remote: &RemoteTool) -> anyhow::Result<Self> {
        let server = connection.name();
        let remote_name = remote.name.to_string();
        let exposed_name = format!("{server}{NAME_SEPARATOR}{remote_name}");

        validate_exposed_name(&exposed_name)?;

        let description = remote
            .description
            .as_ref()
            .map(|description| description.to_string())
            .unwrap_or_else(|| format!("MCP 工具 `{remote_name}`（来自 server `{server}`）"));

        let schema = Value::Object((*remote.input_schema).clone());

        Ok(Self {
            connection,
            remote_name,
            exposed_name,
            description,
            schema,
        })
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let arguments = parse_arguments(args_json)
            .map_err(|err| anyhow::anyhow!("MCP 工具 `{}` 参数无效: {err}", self.exposed_name))?;

        let params = CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments);

        let response = tokio::time::timeout(
            self.connection.timeout(),
            self.connection.service().call_tool_once(params),
        )
        .await
        .map_err(|_| anyhow::anyhow!("MCP 工具 `{}` 调用超时", self.exposed_name))?
        .map_err(|err| anyhow::anyhow!("MCP 工具 `{}` 调用失败: {err}", self.exposed_name))?;

        match response {
            CallToolResponse::Complete(result) => Ok(render_call_result(&result)),
            CallToolResponse::InputRequired(_) => anyhow::bail!(
                "MCP 工具 `{}` 需要交互式输入（MRTR），一期不支持",
                self.exposed_name
            ),
            CallToolResponse::Task(_) => anyhow::bail!(
                "MCP 工具 `{}` 返回了 task（SEP-2663），一期不支持",
                self.exposed_name
            ),
            other => anyhow::bail!("MCP 工具 `{}` 返回未知响应: {other:?}", self.exposed_name),
        }
    }
}

/// 把连接上的全部远端工具转成本地工具；无效工具记录告警后跳过。
pub fn tools_from_connection(connection: &Arc<McpConnection>) -> Vec<Box<dyn Tool>> {
    let remote_tools = connection.tools();
    let mut tools: Vec<Box<dyn Tool>> = Vec::with_capacity(remote_tools.len());

    for remote in remote_tools {
        match McpTool::try_new(Arc::clone(connection), remote) {
            Ok(tool) => tools.push(Box::new(tool)),
            Err(err) => tracing::warn!(
                target: "mcp",
                "跳过 MCP 工具 `{}`（server `{}`）: {err:#}",
                remote.name,
                connection.name()
            ),
        }
    }

    tools
}

/// 解析模型给出的参数：空串视为无参；必须是 JSON 对象。
fn parse_arguments(args_json: &str) -> anyhow::Result<Map<String, Value>> {
    let trimmed = args_json.trim();
    if trimmed.is_empty() {
        return Ok(Map::new());
    }

    let value: Value =
        serde_json::from_str(trimmed).map_err(|err| anyhow::anyhow!("不是合法 JSON: {err}"))?;

    match value {
        Value::Object(map) => Ok(map),
        other => anyhow::bail!("参数必须是 JSON 对象，实际是 {}", json_kind(&other)),
    }
}

/// 把 `CallToolResult` 降维成单条观察文本。
pub fn render_call_result(result: &CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(result.content.len());

    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Image(image) => parts.push(format!(
                "[image {}, {} bytes]",
                image.mime_type,
                image.data.len()
            )),
            ContentBlock::Audio(audio) => parts.push(format!(
                "[audio {}, {} bytes]",
                audio.mime_type,
                audio.data.len()
            )),
            ContentBlock::Resource(resource) => {
                parts.push(format!("[resource {}]", resource_uri(&resource.resource)));
            }
            ContentBlock::ResourceLink(resource) => {
                parts.push(format!("[resource-link {}]", resource.uri));
            }
            other => parts.push(format!("[unsupported content: {other:?}]")),
        }
    }

    let rendered = parts.join("\n");
    if !rendered.trim().is_empty() {
        return rendered;
    }

    if let Some(structured) = &result.structured_content
        && let Ok(text) = serde_json::to_string(structured)
    {
        return text;
    }

    if result.is_error == Some(true) {
        return "工具返回错误但没有内容".to_owned();
    }

    "（工具没有返回内容）".to_owned()
}

fn resource_uri(resource: &ResourceContents) -> &str {
    match resource {
        ResourceContents::TextResourceContents { uri, .. }
        | ResourceContents::BlobResourceContents { uri, .. } => uri,
        _ => "<unknown>",
    }
}

fn validate_exposed_name(exposed_name: &str) -> anyhow::Result<()> {
    if exposed_name.is_empty() {
        anyhow::bail!("MCP 工具名不能为空");
    }

    if !exposed_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("MCP 工具名 `{exposed_name}` 只能包含字母、数字、下划线和连字符");
    }

    let length = exposed_name.chars().count();
    if length > MAX_FUNCTION_NAME_LEN {
        anyhow::bail!("MCP 工具名 `{exposed_name}` 长度 {length} 超过 {MAX_FUNCTION_NAME_LEN}");
    }

    Ok(())
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result_from_json(value: Value) -> CallToolResult {
        serde_json::from_value(value).expect("CallToolResult 反序列化失败")
    }

    #[test]
    fn renders_single_text_block() {
        let result = CallToolResult::success(vec![ContentBlock::text("hello")]);
        assert_eq!(render_call_result(&result), "hello");
    }

    #[test]
    fn joins_multiple_text_blocks() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("line1"),
            ContentBlock::text("line2"),
        ]);
        assert_eq!(render_call_result(&result), "line1\nline2");
    }

    #[test]
    fn renders_image_as_placeholder() {
        let result = CallToolResult::success(vec![ContentBlock::image("AAAA", "image/png")]);
        assert_eq!(render_call_result(&result), "[image image/png, 4 bytes]");
    }

    #[test]
    fn falls_back_to_structured_content() {
        let result = result_from_json(json!({
            "content": [],
            "structuredContent": { "answer": 42 }
        }));
        assert_eq!(render_call_result(&result), r#"{"answer":42}"#);
    }

    #[test]
    fn error_without_content_has_message() {
        let result = CallToolResult::error(Vec::new());
        assert_eq!(render_call_result(&result), "工具返回错误但没有内容");
    }

    #[test]
    fn empty_result_has_placeholder() {
        let result = CallToolResult::success(Vec::new());
        assert_eq!(render_call_result(&result), "（工具没有返回内容）");
    }

    #[test]
    fn empty_arguments_become_empty_object() {
        assert!(parse_arguments("").expect("空串应可行").is_empty());
        assert!(parse_arguments("   ").expect("空白应可行").is_empty());
    }

    #[test]
    fn object_arguments_pass_through() {
        let map = parse_arguments(r#"{"a":1}"#).expect("对象应可行");
        assert_eq!(map.get("a"), Some(&json!(1)));
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        assert!(parse_arguments("[1,2]").is_err());
        assert!(parse_arguments("42").is_err());
        assert!(parse_arguments("\"x\"").is_err());
    }

    #[test]
    fn invalid_json_arguments_are_rejected() {
        assert!(parse_arguments("{oops").is_err());
    }

    #[test]
    fn valid_exposed_names_pass() {
        assert!(validate_exposed_name("filesystem__read_file").is_ok());
        assert!(validate_exposed_name("a-b__c_d").is_ok());
    }

    #[test]
    fn invalid_exposed_names_are_rejected() {
        assert!(validate_exposed_name("").is_err());
        assert!(validate_exposed_name("server__bad.name").is_err());
        assert!(validate_exposed_name("server tool").is_err());
    }

    #[test]
    fn overly_long_exposed_name_is_rejected() {
        let too_long = "a".repeat(MAX_FUNCTION_NAME_LEN + 1);
        assert!(validate_exposed_name(&too_long).is_err());

        let exact = "a".repeat(MAX_FUNCTION_NAME_LEN);
        assert!(validate_exposed_name(&exact).is_ok());
    }

    #[tokio::test]
    #[ignore = "需要本机 python3；手动运行 cargo test -- --ignored"]
    async fn executes_tool_through_adapter() {
        use crate::tools::mcp::config::{McpConfig, McpServerConfig};
        use crate::tools::mcp::connection::connect_all;

        let config = McpConfig {
            servers: [(
                "probe".to_owned(),
                McpServerConfig {
                    command: "python3".to_owned(),
                    args: vec![crate::tools::mcp::FAKE_SERVER_SCRIPT.to_owned()],
                    env: Default::default(),
                    cwd: None,
                    required: false,
                    timeout_secs: 60,
                },
            )]
            .into_iter()
            .collect(),
        };

        let connections = connect_all(&config).await.expect("连接失败");
        assert_eq!(connections.len(), 1);

        let tools = tools_from_connection(&connections[0]);

        let echo = tools
            .iter()
            .find(|tool| tool.name() == "probe__echo")
            .expect("应存在 probe__echo");
        let output = echo
            .execute(r#"{"text":"hi"}"#)
            .await
            .expect("echo 调用失败");
        assert!(output.contains("echo: hi"), "输出 = {output}");
        echo.definition().expect("definition 应成功构造");

        // 工具级错误（isError=true）应返回 Ok 文本，而不是 Err。
        let fail = tools
            .iter()
            .find(|tool| tool.name() == "probe__fail")
            .expect("应存在 probe__fail");
        let output = fail.execute("{}").await.expect("工具级错误不应是 Err");
        assert!(output.contains("boom"), "输出 = {output}");
    }
}
