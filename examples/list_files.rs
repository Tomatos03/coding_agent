//! `list_files` 工具的独立示例：在临时工作区里造一棵目录树，演示单层、递归与 `path` 参数。
//!
//! ```bash
//! cargo run --example list_files
//! ```

use std::path::{Path, PathBuf};

use coding_agent::{
    bootstrap::init,
    tools::{
        local::{ListFiles, Permission},
        tool::Tool,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let root = demo_root("list_files")?;
    let permission = Permission::workspace(&root)?;
    println!("工作区根目录: {}", permission.base().display());

    // 造一棵小目录树：src/{main.rs, bin/gaia.rs} 与 top.txt。
    std::fs::create_dir_all(root.join("src/bin"))?;
    std::fs::write(root.join("src/main.rs"), "")?;
    std::fs::write(root.join("src/bin/gaia.rs"), "")?;
    std::fs::write(root.join("top.txt"), "")?;
    println!("已创建目录树：src/main.rs、src/bin/gaia.rs、top.txt");

    let tool = ListFiles::new(permission);

    // 1) 非递归（默认）：只列根目录这一层，目录以 / 结尾。
    let args = r#"{}"#;
    println!("\n调用 list_files，参数: {args}");
    println!("{}", tool.execute(args).await?);

    // 2) 递归：按名字升序做先序 DFS。
    let args = r#"{"recursive":true}"#;
    println!("\n调用 list_files，参数: {args}");
    println!("{}", tool.execute(args).await?);

    // 3) path 参数：只看子目录。
    let args = r#"{"path":"src"}"#;
    println!("\n调用 list_files，参数: {args}");
    println!("{}", tool.execute(args).await?);

    // 4) 越界路径会被工作区边界拒绝。
    let args = r#"{"path":"../outside"}"#;
    println!("\n调用 list_files，参数: {args}");
    match tool.execute(args).await {
        Ok(output) => println!("{output}"),
        Err(error) => println!("预期内的错误: {error}"),
    }

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
