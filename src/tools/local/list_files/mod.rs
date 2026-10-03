//! 列出工作区内目录条目的工具。
//!
//! 只读、无副作用；输出是给模型看的纯文本清单：每行一个条目，目录名以 `/` 结尾。
//! 大目录（例如 `target/`）会按 [`MAX_ENTRIES`] 截断并明确提示，避免撑爆上下文。

use std::path::{Path, PathBuf};

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::local::permission::Permission;
use crate::tools::tool::Tool;

/// 单次返回的最大条目数。
pub const MAX_ENTRIES: usize = 500;

const DESCRIPTION: &str = "列出工作区内目录条目，目录名以 / 结尾；可选 recursive 递归展开。";

/// 遍历中的待处理项。
///
/// 用显式栈而不是递归：`execute` 是 async，async 递归需要装箱，显式栈更直接，
/// 也方便在达到上限时立刻停下。
enum Pending {
    /// 一个目录：输出它本身（根目录除外），并按 `expand` 决定是否展开子项。
    Dir {
        absolute: PathBuf,
        relative: PathBuf,
        expand: bool,
    },
    /// 一个文件或符号链接：只输出显示路径。
    Leaf { display: String },
}

pub struct ListFiles {
    permission: Permission,
    description: String,
}

impl ListFiles {
    pub fn new(permission: Permission) -> Self {
        let description = format!("{DESCRIPTION}{}", permission.scope_note());
        Self {
            permission,
            description,
        }
    }
}

#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct ListFilesArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "要列出的目录，相对于工作区根目录；默认 \".\"，即工作区根目录。")]
    pub path: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "是否递归展开子目录。默认 false，只列一层。")]
    pub recursive: Option<bool>,
}

#[async_trait::async_trait]
impl Tool for ListFiles {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        // schema 是静态的，序列化失败属于编程错误。
        serde_json::to_value(schema_for!(ListFilesArgs))
            .expect("failed to serialize ListFilesArgs schema")
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: ListFilesArgs = serde_json::from_str(args_json).map_err(|e| {
            anyhow::anyhow!("[{}] Failed to deserialize arguments: {e}", self.name())
        })?;

        let requested = args.path.as_deref().unwrap_or(".");
        let dir = self.permission.resolve_existing(requested)?;
        if !tokio::fs::metadata(&dir).await?.is_dir() {
            anyhow::bail!(
                "[{}] `{requested}` 不是目录，读取文件请用 read_file",
                self.name()
            );
        }

        let recursive = args.recursive.unwrap_or(false);
        let mut outputs: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut stack = vec![Pending::Dir {
            absolute: dir,
            relative: PathBuf::new(),
            // 根目录始终要展开（哪怕 non-recursive 也要列出它这一层）。
            expand: true,
        }];

        while let Some(pending) = stack.pop() {
            match pending {
                Pending::Leaf { display } => {
                    if outputs.len() >= MAX_ENTRIES {
                        truncated = true;
                        break;
                    }
                    outputs.push(display);
                }
                Pending::Dir {
                    absolute,
                    relative,
                    expand,
                } => {
                    if !relative.as_os_str().is_empty() {
                        if outputs.len() >= MAX_ENTRIES {
                            truncated = true;
                            break;
                        }
                        outputs.push(format!("{}/", relative.display()));
                    }

                    if !expand {
                        continue;
                    }

                    let children = read_sorted(&absolute).await?;
                    // 反向入栈：栈是 LIFO，这样弹出顺序与名字升序一致，
                    // 且目录会紧跟着它的子项（真正的先序 DFS）。
                    for child in children.into_iter().rev() {
                        let file_type = child.file_type().await?;
                        let relative_child = relative.join(child.file_name());
                        if file_type.is_dir() {
                            stack.push(Pending::Dir {
                                absolute: child.path(),
                                relative: relative_child,
                                expand: recursive,
                            });
                        } else {
                            // 符号链接的 file_type 是 symlink，不会被当成目录展开，
                            // 因此既不会逃出工作区，也不会形成环。
                            stack.push(Pending::Leaf {
                                display: relative_child.display().to_string(),
                            });
                        }
                    }
                }
            }
        }

        if outputs.is_empty() {
            return Ok("（空目录）".to_owned());
        }

        let mut rendered = outputs.join("\n");
        if truncated {
            rendered.push_str(&format!("\n…（已截断，仅显示前 {MAX_ENTRIES} 项）"));
        }
        Ok(rendered)
    }
}

/// 读取目录并按文件名升序排序；顺序固定，输出才可复现。
async fn read_sorted(dir: &Path) -> std::io::Result<Vec<tokio::fs::DirEntry>> {
    let mut reader = tokio::fs::read_dir(dir).await?;
    let mut entries = Vec::new();
    while let Some(entry) = reader.next_entry().await? {
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::local::test_support::TempDir;

    fn tool_for(dir: &TempDir) -> ListFiles {
        let permission = Permission::workspace(dir.path()).expect("构造工作区失败");
        ListFiles::new(permission)
    }

    fn lines(output: &str) -> Vec<&str> {
        output.lines().collect()
    }

    #[tokio::test]
    async fn lists_sorted_with_directory_suffix() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join("src")).expect("建目录失败");
        std::fs::write(dir.path().join("Cargo.toml"), "x").expect("写文件失败");
        std::fs::write(dir.path().join("README.md"), "x").expect("写文件失败");

        let output = tool_for(&dir).execute("{}").await.expect("应成功");

        assert_eq!(lines(&output), vec!["Cargo.toml", "README.md", "src/"]);
    }

    #[tokio::test]
    async fn non_recursive_does_not_descend() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.path().join("src/bin")).expect("建目录失败");
        std::fs::write(dir.path().join("src/main.rs"), "").expect("写文件失败");
        std::fs::write(dir.path().join("src/bin/gaia.rs"), "").expect("写文件失败");

        let output = tool_for(&dir).execute("{}").await.expect("应成功");

        assert_eq!(lines(&output), vec!["src/"]);
    }

    #[tokio::test]
    async fn recursive_lists_in_pre_order() {
        let dir = TempDir::new();
        println!("{:?}", dir.path());
        std::fs::create_dir_all(dir.path().join("src/bin")).expect("建目录失败");
        std::fs::write(dir.path().join("src/main.rs"), "").expect("写文件失败");
        std::fs::write(dir.path().join("src/bin/gaia.rs"), "").expect("写文件失败");
        std::fs::write(dir.path().join("top.txt"), "").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"recursive":true}"#)
            .await
            .expect("应成功");

        assert_eq!(
            lines(&output),
            vec![
                "src/",
                "src/bin/",
                "src/bin/gaia.rs",
                "src/main.rs",
                "top.txt",
            ]
        );
    }

    #[tokio::test]
    async fn lists_subdirectory_via_path_argument() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join("src")).expect("建目录失败");
        std::fs::write(dir.path().join("src/lib.rs"), "").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"src"}"#)
            .await
            .expect("应成功");

        assert_eq!(lines(&output), vec!["lib.rs"]);
    }

    #[tokio::test]
    async fn empty_directory_has_placeholder() {
        let dir = TempDir::new();

        let output = tool_for(&dir).execute("{}").await.expect("应成功");

        assert_eq!(output, "（空目录）");
    }

    #[tokio::test]
    async fn rejects_file_path() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");

        let error = tool_for(&dir)
            .execute(r#"{"path":"a.txt"}"#)
            .await
            .expect_err("文件路径应报错");

        assert!(
            format!("{error:#}").contains("不是目录"),
            "错误 = {error:#}"
        );
    }

    #[tokio::test]
    async fn rejects_escaping_path() {
        let dir = TempDir::new();

        assert!(
            tool_for(&dir)
                .execute(r#"{"path":"../outside"}"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_malformed_arguments() {
        let dir = TempDir::new();

        assert!(tool_for(&dir).execute("not json").await.is_err());
    }

    #[tokio::test]
    async fn truncates_at_max_entries() {
        let dir = TempDir::new();
        for index in 0..(MAX_ENTRIES + 1) {
            std::fs::write(dir.path().join(format!("f{index:04}.txt")), "").expect("写文件失败");
        }

        let output = tool_for(&dir).execute("{}").await.expect("应成功");
        let listed = lines(&output);

        assert_eq!(
            listed.len(),
            MAX_ENTRIES + 1,
            "应有 MAX_ENTRIES 条 + 1 行提示"
        );
        assert!(
            listed[MAX_ENTRIES].contains("已截断"),
            "末行 = {}",
            listed[MAX_ENTRIES]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("secret.txt"), "s").expect("写文件失败");
        symlink(outside.path(), dir.path().join("link")).expect("建符号链接失败");

        let output = tool_for(&dir)
            .execute(r#"{"recursive":true}"#)
            .await
            .expect("应成功");

        assert_eq!(lines(&output), vec!["link"], "不应展开符号链接");
    }

    #[tokio::test]
    async fn full_permission_lists_outside_workspace() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("x.txt"), "x").expect("写文件失败");
        let tool = ListFiles::new(Permission::full(dir.path()).expect("构造 full 权限失败"));

        let args = format!(r#"{{"path":"{}"}}"#, outside.path().display());
        let output = tool.execute(&args).await.expect("应成功");

        assert_eq!(lines(&output), vec!["x.txt"]);
    }
}
