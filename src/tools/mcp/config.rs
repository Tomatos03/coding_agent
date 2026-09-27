use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

/// 固定读取的 MCP 配置文件路径（当前按需求写死，不提供环境变量覆盖）。
pub const CONFIG_PATH: &str = "mcp.json";

/// 每次 `tools/call` 的默认超时秒数。
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// MCP 顶层配置，对应 `{ "mcpServers": { ... } }`。
///
/// 使用 `deny_unknown_fields` 让拼写错误（如把 `args` 写成 `arg`）在启动时立即暴露，
/// 而不是被静默忽略。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default, rename = "mcpServers")]
    pub servers: HashMap<String, McpServerConfig>, // server_name -> config
}

/// 单个 MCP server 的启动配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// 可执行文件（如 `npx`、`uvx` 或绝对路径）。
    pub command: String,

    #[serde(default)]
    pub args: Vec<String>,

    /// 追加/覆盖到继承的父进程环境变量之上。
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// 子进程工作目录；缺省沿用父进程当前目录。
    #[serde(default)]
    pub cwd: Option<PathBuf>,

    /// 为 `true` 时该 server 启动失败会让整体启动失败；默认 `false`（跳过并告警）。
    #[serde(default)]
    pub required: bool,

    #[serde(default = "default_timeout_secs", rename = "timeoutSecs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl McpServerConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// 解析并校验配置文本。纯函数，便于单元测试。
pub fn parse_config(raw: &str) -> anyhow::Result<McpConfig> {
    let config: McpConfig = serde_json::from_str(raw).context("解析 MCP 配置 JSON 失败")?;
    validate(&config)?;
    Ok(config)
}

/// 语义校验：JSON 能解析不代表配置可用。
pub fn validate(config: &McpConfig) -> anyhow::Result<()> {
    for (name, server) in &config.servers {
        validate_server_name(name)?;

        if server.command.trim().is_empty() {
            anyhow::bail!("MCP server `{name}` 的 command 不能为空");
        }
        if server.timeout_secs == 0 {
            anyhow::bail!("MCP server `{name}` 的 timeoutSecs 必须大于 0");
        }
    }
    Ok(())
}

/// server 名最终会参与拼装 OpenAI function 名（`{server}__{tool}`），
/// 因此这里先限制在函数名允许的字符集内。
fn validate_server_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("MCP server 名不能为空");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("MCP server 名 `{name}` 只能包含字母、数字、下划线和连字符");
    }
    Ok(())
}

/// 从指定路径加载配置。文件不存在视为「未启用 MCP」，返回空配置。
pub fn load_config_from(path: &Path) -> anyhow::Result<McpConfig> {
    if !path.exists() {
        tracing::info!(target: "mcp", "未找到 MCP 配置文件 {}，跳过 MCP", path.display());
        return Ok(McpConfig::default());
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取 MCP 配置文件失败: {}", path.display()))?;

    parse_config(&raw).with_context(|| format!("加载 MCP 配置文件失败: {}", path.display()))
}

/// 从固定路径 [`CONFIG_PATH`] 加载配置。
pub fn load_config() -> anyhow::Result<McpConfig> {
    load_config_from(Path::new(CONFIG_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "mcpServers": {
            "filesystem": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                "env": { "TOKEN": "abc" },
                "cwd": "/tmp",
                "required": true,
                "timeoutSecs": 30
            }
        }
    }"#;

    #[test]
    fn parses_full_config() {
        let config = parse_config(FULL).expect("完整配置应解析成功");
        let server = config.servers.get("filesystem").expect("应存在 filesystem");

        assert_eq!(server.command, "npx");
        assert_eq!(
            server.args,
            vec!["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        );
        assert_eq!(server.env.get("TOKEN").map(String::as_str), Some("abc"));
        assert_eq!(server.cwd.as_deref(), Some(Path::new("/tmp")));
        assert!(server.required);
        assert_eq!(server.timeout_secs, 30);
        assert_eq!(server.timeout(), Duration::from_secs(30));
    }

    #[test]
    fn applies_defaults() {
        let config =
            parse_config(r#"{"mcpServers":{"s":{"command":"echo"}}}"#).expect("缺省字段应有默认值");
        let server = config.servers.get("s").expect("应存在 s");

        assert!(server.args.is_empty());
        assert!(server.env.is_empty());
        assert!(server.cwd.is_none());
        assert!(!server.required);
        assert_eq!(server.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn empty_config_is_allowed() {
        assert!(parse_config("{}").expect("空对象应可用").servers.is_empty());
        assert!(
            parse_config(r#"{"mcpServers":{}}"#)
                .expect("空 server 表应可用")
                .servers
                .is_empty()
        );
    }

    #[test]
    fn invalid_json_is_rejected() {
        assert!(parse_config("not json").is_err());
    }

    #[test]
    fn missing_command_is_rejected() {
        let err = parse_config(r#"{"mcpServers":{"s":{}}}"#).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("command"),
            "错误信息应提到 command: {message}"
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err =
            parse_config(r#"{"mcpServers":{"s":{"command":"echo","arg":["x"]}}}"#).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("unknown field") || message.contains("arg"),
            "未知字段应报错: {message}"
        );
    }

    #[test]
    fn invalid_server_name_is_rejected() {
        let err = parse_config(r#"{"mcpServers":{"bad name":{"command":"echo"}}}"#).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("bad name"),
            "错误信息应包含非法名字: {message}"
        );
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let err =
            parse_config(r#"{"mcpServers":{"s":{"command":"echo","timeoutSecs":0}}}"#).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("timeoutSecs"),
            "错误信息应提到 timeoutSecs: {message}"
        );
    }

    #[test]
    fn blank_command_is_rejected() {
        let err = parse_config(r#"{"mcpServers":{"s":{"command":"   "}}}"#).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("command"),
            "错误信息应提到 command: {message}"
        );
    }

    #[test]
    fn missing_file_yields_empty_config() {
        let path = Path::new("__mcp_config_should_not_exist__.json");
        let config = load_config_from(path).expect("文件不存在不应报错");
        assert!(config.servers.is_empty());
    }
}
