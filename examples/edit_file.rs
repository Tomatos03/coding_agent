//! `edit_file` 工具的独立示例：演示基于 4 字母行锚点的定位替换，
//! 以及由 `read_file` 服务记录驱动的**行级冲突检测**。
//!
//! ```bash
//! cargo run --example edit_file
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use coding_agent::{
    bootstrap::init,
    tools::{
        local::{AnchorRegistry, EditFile, Permission, ReadFile, SharedAnchors},
        tool::Tool,
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let root = demo_root("edit_file")?;
    let permission = Permission::workspace(&root)?;
    println!("工作区根目录: {}", permission.base().display());

    std::fs::write(
        root.join("demo.rs"),
        "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n",
    )?;

    // `read_file` 与 `edit_file` 必须共享同一份锚点账本，服务记录才能被看到。
    let anchors: SharedAnchors = Arc::new(Mutex::new(AnchorRegistry::new(64)));
    let reader = ReadFile::with_anchors(permission.clone(), anchors.clone());
    let editor = EditFile::with_anchors(permission, anchors);

    // 1) 带锚点读取：每行 `ANCHOR│内容`，展示过的行被登记为服务记录。
    let listing = reader
        .execute(r#"{"path":"demo.rs","line_anchors":true}"#)
        .await?;
    println!("read_file（带锚点）:\n{listing}");

    // 2) 用锚点替换 `let x = 1;` 那一行。
    let target = extract_anchor(&listing, "let x = 1;");
    let args = format!(
        r#"{{"command":"replace_anchor","path":"demo.rs","remove_from":"{target}","replacement_lines":["    let x = 42;"]}}"#
    );
    println!("\n调用 edit_file（replace_anchor），参数: {args}");
    println!("{}", editor.execute(&args).await?);

    // 3) 再次读取：刷新服务记录，并拿到当前锚点。
    let listing = reader
        .execute(r#"{"path":"demo.rs","line_anchors":true}"#)
        .await?;
    println!("\nread_file（再次带锚点）:\n{listing}");
    let first = extract_anchor(&listing, "fn main()");
    let last = extract_anchor(&listing, "println!");

    // 4) 模拟外部改动：模型没读到这一步，服务记录已过期。
    let content = std::fs::read_to_string(root.join("demo.rs"))?;
    std::fs::write(
        root.join("demo.rs"),
        content.replace("let x = 42;", "let x = 999;"),
    )?;
    println!("\n（模拟外部改动：let x = 42; -> let x = 999;）");

    // 5) 用首尾锚点替换整个区间：中间行已变，冲突被拒绝并回传当前范围。
    let args = format!(
        r#"{{"command":"replace_anchor","path":"demo.rs","remove_from":"{first}","remove_to":"{last}","replacement_lines":["L1","L2","L3"]}}"#
    );
    println!("\n调用 edit_file（区间内有过期行），参数: {args}");
    match editor.execute(&args).await {
        Ok(output) => println!("{output}"),
        Err(error) => println!("预期内的冲突: {error}"),
    }

    println!(
        "\n最终 demo.rs（冲突被拒，文件保持外部改动后的内容）:\n{}",
        std::fs::read_to_string(root.join("demo.rs"))?
    );

    cleanup(&root);
    Ok(())
}

/// 从带锚点的 `read_file` 输出里，取出包含 `needle` 的那一行的 `ANCHOR│` 前 4 个字母。
fn extract_anchor(output: &str, needle: &str) -> String {
    output
        .lines()
        .find(|line| line.contains(needle))
        .and_then(|line| line.split_once('│').map(|(anchor, _)| anchor.to_owned()))
        .expect("应能找到带锚点的目标行")
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
