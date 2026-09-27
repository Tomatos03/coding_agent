use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use rmcp::ServiceExt;
use rmcp::model::Tool;
use rmcp::service::{RoleClient, RunningService};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::time::timeout;

use crate::tools::mcp::config::{McpConfig, McpServerConfig};

/// 与单个 server 完成 `initialize` 握手与 `tools/list` 的启动阶段超时。
///
/// 与 `McpServerConfig.timeout_secs`（每次 `tools/call` 的调用超时）用途不同。
pub const HANDSHAKE_TIMEOUT_SECS: u64 = 30;

/// 一个已连接的 MCP server：MCP Client + 工具快照 + 子进程句柄。
pub struct McpConnection {
    name: String,
    service: RunningService<RoleClient, ()>,
    tools: Vec<Tool>,
    /// 每次 `tools/call` 的超时，来自 server 配置。
    timeout: Duration,
    /// 仅用于 keep-alive：持有它可避免 `kill_on_drop` 提前杀死子进程。
    _child: Child,
}

impl McpConnection {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn service(&self) -> &RunningService<RoleClient, ()> {
        &self.service
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// 连接单个 MCP server，完成握手并拉取工具列表。
pub async fn connect(name: &str, config: &McpServerConfig) -> anyhow::Result<McpConnection> {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .envs(&config.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("启动 MCP server `{name}` 失败: {}", config.command))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("MCP server `{name}` 缺少 stdin 管道"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("MCP server `{name}` 缺少 stdout 管道"))?;

    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_logger(name, stderr);
    }

    let handshake_timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

    let service: RunningService<RoleClient, ()> =
        timeout(handshake_timeout, ().serve((stdout, stdin)))
            .await
            .map_err(|_| {
                anyhow::anyhow!("MCP server `{name}` 初始化握手超时（>{HANDSHAKE_TIMEOUT_SECS}s）")
            })?
            .map_err(|err| anyhow::anyhow!("MCP server `{name}` 初始化握手失败: {err}"))?;

    let tools = timeout(handshake_timeout, service.list_all_tools())
        .await
        .map_err(|_| {
            anyhow::anyhow!("MCP server `{name}` tools/list 超时（>{HANDSHAKE_TIMEOUT_SECS}s）")
        })?
        .map_err(|err| anyhow::anyhow!("MCP server `{name}` tools/list 失败: {err}"))?;

    Ok(McpConnection {
        name: name.to_owned(),
        service,
        tools,
        timeout: config.timeout(),
        _child: child,
    })
}

/// 逐个连接配置中的 server，并按 `required` 做失败隔离。
///
/// - 连接失败且 `required = false`：记录告警后跳过。
/// - 连接失败且 `required = true`：返回错误。
/// - 连接成功但没有工具：记录告警并丢弃该连接（回收子进程）。
pub async fn connect_all(config: &McpConfig) -> anyhow::Result<Vec<Arc<McpConnection>>> {
    // HashMap 迭代无序，排序以保证日志与测试的确定性。
    let mut servers: Vec<(&String, &McpServerConfig)> = config.servers.iter().collect();
    servers.sort_by(|a, b| a.0.cmp(b.0));

    let mut connections = Vec::new();
    for (name, server_config) in servers {
        match connect(name, server_config).await {
            Ok(connection) if connection.tools().is_empty() => {
                tracing::warn!(target: "mcp", "MCP server `{name}` 未提供任何工具，已丢弃该连接");
            }
            Ok(connection) => {
                tracing::info!(
                    target: "mcp",
                    "已连接 MCP server `{name}`，发现 {} 个工具",
                    connection.tools().len()
                );
                connections.push(Arc::new(connection));
            }
            Err(err) => {
                if server_config.required {
                    return Err(err.context(format!("必需的 MCP server `{name}` 连接失败")));
                }
                tracing::warn!(target: "mcp", "MCP server `{name}` 连接失败，已跳过: {err:#}");
            }
        }
    }

    Ok(connections)
}

/// 把 server 的 stderr 逐行转发到 tracing，避免管道写满导致子进程阻塞。
fn spawn_stderr_logger(name: &str, stderr: ChildStderr) {
    let name = name.to_owned();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    tracing::debug!(target: "mcp", "server `{name}` stderr: {line}");
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(target: "mcp", "server `{name}` stderr 读取结束: {err}");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(command: &str, required: bool) -> McpServerConfig {
        McpServerConfig {
            command: command.to_owned(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            required,
            timeout_secs: 60,
        }
    }

    fn config_with(name: &str, server: McpServerConfig) -> McpConfig {
        McpConfig {
            servers: [(name.to_owned(), server)].into_iter().collect(),
        }
    }

    #[test]
    fn connection_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<McpConnection>();
        assert_send_sync::<Arc<McpConnection>>();
    }

    #[tokio::test]
    async fn empty_config_yields_no_connections() {
        let connections = connect_all(&McpConfig::default())
            .await
            .expect("空配置不应报错");
        assert!(connections.is_empty());
    }

    #[tokio::test]
    async fn missing_binary_is_skipped_when_not_required() {
        let config = config_with(
            "nope",
            server("definitely-not-a-real-mcp-binary-12345", false),
        );

        let connections = connect_all(&config)
            .await
            .expect("非必需 server 失败应被跳过");
        assert!(connections.is_empty());
    }

    #[tokio::test]
    async fn missing_binary_fails_when_required() {
        let config = config_with(
            "nope",
            server("definitely-not-a-real-mcp-binary-12345", true),
        );

        assert!(
            connect_all(&config).await.is_err(),
            "必需 server 失败应报错"
        );
    }

    #[tokio::test]
    #[ignore = "需要本机 python3；手动运行 cargo test -- --ignored"]
    async fn discovers_tools_from_fake_server() {
        let mut fake = server("python3", false);
        fake.args = vec![crate::tools::mcp::FAKE_SERVER_SCRIPT.to_owned()];

        let connections = connect_all(&config_with("fake", fake))
            .await
            .expect("应连接成功");

        assert_eq!(connections.len(), 1);
        let names: Vec<&str> = connections[0]
            .tools()
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect();
        assert!(names.contains(&"echo"), "工具列表应含 echo: {names:?}");
        assert!(names.contains(&"add"), "工具列表应含 add: {names:?}");
    }
}
