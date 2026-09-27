use std::collections::HashMap;

use crate::tools::{
    local::WebSearch,
    mcp::{McpConfig, connect_all, load_config, tools_from_connection},
    tool::Tool,
};

pub mod local;
pub mod mcp;
pub mod tool;

pub type ToolHashMap = HashMap<String, Box<dyn Tool>>;

/// 启动时构建工具表：本地工具 + `mcp.json` 中配置的 MCP 工具。
///
/// - `mcp.json` 不存在：只注册本地工具。
/// - `mcp.json` 非法：返回 `Err`（fail fast）。
/// - 某个 server 连接失败：由 [`connect_all`] 按 `required` 决定跳过还是报错。
pub async fn build_tools() -> anyhow::Result<ToolHashMap> {
    let config = load_config()?;
    build_tools_with(config).await
}

/// 直接吃配置构建工具表（不读文件），便于测试与嵌入方复用。
pub async fn build_tools_with(config: McpConfig) -> anyhow::Result<ToolHashMap> {
    let mut registry = ToolHashMap::new();
    // 优先创建本地工具
    let local_count = insert_tools(&mut registry, local_tools());

    let connections = connect_all(&config).await?;
    let mut mcp_count = 0;
    for connection in &connections {
        mcp_count += insert_tools(&mut registry, tools_from_connection(connection));
    }

    if !config.servers.is_empty() {
        tracing::info!(
            target: "mcp",
            "MCP 就绪：配置 {} 个 server，连接成功 {} 个，注册 {mcp_count} 个 MCP 工具；本地工具 {local_count} 个",
            config.servers.len(),
            connections.len()
        );
    }

    Ok(registry)
}

fn local_tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(WebSearch)]
}

/// 先到先得：重名工具跳过并告警（本地工具先注册，因此本地优先）。
///
/// 返回实际插入的工具数量（被跳过的重名工具不计入）。
fn insert_tools(registry: &mut ToolHashMap, tools: Vec<Box<dyn Tool>>) -> usize {
    let mut inserted = 0;
    for tool in tools {
        let name = tool.name().to_owned();
        if registry.contains_key(&name) {
            tracing::warn!(target: "mcp", "工具名 `{name}` 冲突，跳过重复注册");
            continue;
        }
        registry.insert(name, tool);
        inserted += 1;
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    struct DummyTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "测试用假工具"
        }

        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, _args_json: &str) -> anyhow::Result<String> {
            Ok("ok".to_owned())
        }
    }

    #[test]
    fn duplicate_tool_names_keep_the_first() {
        let mut registry = ToolHashMap::new();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(DummyTool("x")), Box::new(DummyTool("x"))];

        insert_tools(&mut registry, tools);

        assert_eq!(registry.len(), 1, "重名工具应只保留一个");
        assert_eq!(
            registry.get("x").map(|tool| tool.description()),
            Some("测试用假工具")
        );
    }

    #[tokio::test]
    async fn empty_config_yields_local_tools_only() {
        let registry = build_tools_with(McpConfig::default())
            .await
            .expect("空配置应成功");

        assert!(registry.contains_key("web_search"), "应注册本地 web_search");
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    #[ignore = "需要本机 python3；手动运行 cargo test -- --ignored"]
    async fn registry_keeps_mcp_connection_alive() {
        use crate::tools::mcp::{McpConfig, McpServerConfig};

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

        let registry = build_tools_with(config).await.expect("构建注册表失败");
        assert!(registry.contains_key("probe__echo"), "应注册 probe__echo");

        // `connections` 已在 build_tools_with 内被 drop；
        // 这里验证 McpTool 持有的 Arc<McpConnection> 仍让子进程存活。
        let echo = registry.get("probe__echo").expect("应有 probe__echo");
        let output = echo
            .execute(r#"{"text":"still alive"}"#)
            .await
            .expect("调用失败");
        assert!(output.contains("still alive"), "输出 = {output}");
    }
}
