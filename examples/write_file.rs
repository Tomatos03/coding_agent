//! `write_file` 工具的独立示例：演示自动建父目录、新建与覆盖。
//!
//! ```bash
//! cargo run --example write_file
//! ```

use std::path::{Path, PathBuf};

use coding_agent::{
    bootstrap::init,
    tools::{
        local::{Permission, WriteFile},
        tool::Tool,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let root = demo_root("write_file")?;
    let permission = Permission::workspace(&root)?;
    println!("工作区根目录: {}", permission.base().display());

    let tool = WriteFile::new(permission);

    // 1) 目标父目录不存在：write_file 会自动创建。
    let args = r#"{"path":"notes/greeting.txt","content":"hello from write_file"}"#;
    println!("\n调用 write_file，参数: {args}");
    println!("{}", tool.execute(args).await?);

    // 2) 再写一次同一路径：整文件覆盖。
    let args = r#"{"path":"notes/greeting.txt","content":"updated"}"#;
    println!("\n调用 write_file，参数: {args}");
    println!("{}", tool.execute(args).await?);

    // 用 std::fs 复核磁盘上的真实内容。
    let written = std::fs::read_to_string(root.join("notes/greeting.txt"))?;
    println!("\n磁盘内容: {written:?}");

    cleanup(&root);
    Ok(())
}

fn demo_root(name: &str) -> anyhow::Result<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "coding_agent_example_{name}_{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}
