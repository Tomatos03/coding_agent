//! MCP（Model Context Protocol）支持，当前仅实现 **stdio 传输**与 **tools 能力**。
//!
//! 分层：
//! - [`config`]：读取并校验 `mcp.json`；
//! - [`connection`]：为每个 server 启动子进程、完成握手、发现工具；
//! - [`tool`]：把远端工具适配成本项目的 [`Tool`](crate::tools::tool::Tool)。
//!
//! 角色映射（对应 MCP 规范）：本项目是 Host；[`connection`] 里每个 `RunningService`
//! 是一个 MCP Client；被启动的子进程是 MCP Server。

pub mod config;
pub mod connection;
pub mod tool;

pub use config::{McpConfig, McpServerConfig, load_config};
pub use connection::{McpConnection, connect, connect_all};
pub use tool::{McpTool, render_call_result, tools_from_connection};

/// 集成测试使用的假 MCP server 脚本路径。
///
/// 用 `CARGO_MANIFEST_DIR` 拼绝对路径，避免测试运行器的 CWD 不是 crate 根时找不到脚本。
#[cfg(test)]
pub(crate) const FAKE_SERVER_SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/fake_mcp_server.py"
);
