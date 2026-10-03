//! `delete_files` 工具的独立示例：批量删除文件、目录递归删除，以及校验失败时一个都不删。
//!
//! ```bash
//! cargo run --example delete_files
//! ```

use std::path::{Path, PathBuf};

use coding_agent::{
    bootstrap::init,
    tools::{
        local::{DeleteFiles, Permission},
        tool::Tool,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let root = demo_root("delete_files")?;
    let permission = Permission::workspace(&root)?;
    println!("工作区根目录: {}", permission.base().display());

    std::fs::write(root.join("a.txt"), "a")?;
    std::fs::write(root.join("b.txt"), "b")?;
    std::fs::write(root.join("keep.txt"), "keep")?;
    std::fs::create_dir_all(root.join("sub/nested"))?;
    std::fs::write(root.join("sub/nested/deep.txt"), "deep")?;
    println!("初始条目: {:?}", entries(&root));

    let tool = DeleteFiles::new(permission);

    // 1) 批量删除文件。
    let args = r#"{"paths":["a.txt","b.txt"]}"#;
    println!("\n调用 delete_files，参数: {args}");
    println!("{}", tool.execute(args).await?);

    // 2) 目录按文件一样处理：递归删除整棵树。
    let args = r#"{"paths":["sub"]}"#;
    println!("\n调用 delete_files，参数: {args}");
    println!("{}", tool.execute(args).await?);

    // 3) 只要有一项校验不过，就一个都不删。
    let args = r#"{"paths":["keep.txt","missing.txt"]}"#;
    println!("\n调用 delete_files，参数: {args}");
    match tool.execute(args).await {
        Ok(output) => println!("{output}"),
        Err(error) => println!("预期内的错误: {error}"),
    }

    println!("\n剩余条目: {:?}", entries(&root));

    cleanup(&root);
    Ok(())
}

fn entries(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("读目录失败")
        .map(|entry| {
            entry
                .expect("目录项")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
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
