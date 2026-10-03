//! `read_file` 工具的示例：读取命令行指定的文件（可带 offset/limit/anchors），不经过 LLM。
//!
//! ```bash
//! cargo run --example read_file -- src/lib.rs
//! cargo run --example read_file -- src/lib.rs --offset 10 --limit 20
//! cargo run --example read_file -- src/lib.rs --anchors
//! ```
//!
//! 目标路径受文件权限约束：只能用 workspace 模式读工作区根目录
//! （固定为进程当前目录，即 agent 的运行目录）内的文件，
//! `..`、根外绝对路径、指向根外的符号链接都会被拒绝。

use coding_agent::{
    bootstrap::init,
    tools::{
        local::{Permission, ReadFile},
        tool::Tool,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        print_usage();
        return Ok(());
    };
    if matches!(first.as_str(), "-h" | "--help") {
        print_usage();
        return Ok(());
    }

    let (path, offset, limit, anchors) = parse_args(first, args)?;

    let permission = Permission::current_dir()?;
    println!("工作区根目录: {}", permission.base().display());

    let tool = ReadFile::new(permission);

    let mut payload = serde_json::Map::new();
    payload.insert("path".to_owned(), serde_json::json!(path));
    if let Some(offset) = offset {
        payload.insert("offset".to_owned(), serde_json::json!(offset));
    }
    if let Some(limit) = limit {
        payload.insert("limit".to_owned(), serde_json::json!(limit));
    }
    if anchors {
        payload.insert("line_anchors".to_owned(), serde_json::json!(true));
    }
    let arguments = serde_json::Value::Object(payload).to_string();

    println!("调用 read_file，参数: {arguments}\n");
    println!("{}", tool.execute(&arguments).await?);
    Ok(())
}

/// 解析第一个位置参数（路径）与其余选项。
fn parse_args(
    path: String,
    args: impl Iterator<Item = String>,
) -> anyhow::Result<(String, Option<usize>, Option<usize>, bool)> {
    if path.starts_with('-') {
        anyhow::bail!("第一个参数必须是 `<path>`；用 --help 查看用法");
    }

    let mut offset = None;
    let mut limit = None;
    let mut anchors = false;
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--offset" | "-o" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("`{arg}` 需要一个数值"))?;
                offset = Some(
                    value
                        .parse::<usize>()
                        .map_err(|error| anyhow::anyhow!("offset 非法: {error}"))?,
                );
            }
            "--limit" | "-l" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("`{arg}` 需要一个数值"))?;
                limit = Some(
                    value
                        .parse::<usize>()
                        .map_err(|error| anyhow::anyhow!("limit 非法: {error}"))?,
                );
            }
            "--anchors" | "-a" => anchors = true,
            other => anyhow::bail!("未知选项 `{other}`；用 --help 查看用法"),
        }
    }

    Ok((path, offset, limit, anchors))
}

fn print_usage() {
    eprintln!(
        "用法:\n  \
         read_file <path> [--offset N] [--limit N] [--anchors]\n\n\
         说明:\n  \
         <path> 相对工作区根目录（进程当前目录，即 agent 的运行目录）。\n  \
         --offset/-o   起始行号，从 1 开始\n  \
         --limit/-l    最多返回的行数，默认 2000，上限 5000\n  \
         --anchors/-a  每行附带 4 字母行锚点，格式 `ANCHOR│内容`"
    );
}
