//! 手动验证入口：连接 stdio MCP server，适配成本地工具，并调用一次 echo。
//!
//! 用法：
//!   cargo run --example mcp_probe            # 默认用 tests/fixtures 下的假 server
//!   cargo run --example mcp_probe -- npx -y @modelcontextprotocol/server-everything

use coding_agent::tools::mcp::{McpConfig, McpServerConfig, connect_all, tools_from_connection};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    coding_agent::bootstrap::init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        args = vec![
            "python3".to_owned(),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/fake_mcp_server.py"
            )
            .to_owned(),
        ];
    }
    let command = args.remove(0);

    let config = McpConfig {
        servers: [(
            "probe".to_owned(),
            McpServerConfig {
                command,
                args,
                env: Default::default(),
                cwd: None,
                required: false,
                timeout_secs: 60,
            },
        )]
        .into_iter()
        .collect(),
    };

    let connections = connect_all(&config).await?;
    if connections.is_empty() {
        println!("未连接任何 MCP server（详见上方日志）");
        return Ok(());
    }

    for connection in &connections {
        let tools = tools_from_connection(connection);
        println!(
            "server `{}` 适配出 {} 个工具：",
            connection.name(),
            tools.len()
        );
        for tool in &tools {
            println!("  - {} :: {}", tool.name(), tool.description());
        }

        if let Some(echo) = tools.iter().find(|tool| tool.name().ends_with("__echo")) {
            let output = echo.execute(r#"{"text":"hello mcp"}"#).await?;
            println!("调用 {} -> {output}", echo.name());
        }
    }

    Ok(())
}
