//! 把内容整体写入工作区内文件的工具（新建或覆盖）。
//!
//! 只做整文件替换；需要追加时先用 `read_file` 取回原文，重组后整体写回，
//! 或用 `edit_file` 的 `insert` 在末行后插入。

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::local::anchor_registry::{AnchorRegistry, DEFAULT_FILE_CAP, SharedAnchors};
use crate::tools::local::permission::Permission;
use crate::tools::tool::Tool;

const DESCRIPTION: &str =
    "把内容整体写入工作区内文件（新建或覆盖），自动创建缺失父目录；追加请用 edit_file。";

pub struct WriteFile {
    permission: Permission,
    anchors: SharedAnchors,
    description: String,
}

impl WriteFile {
    pub fn new(permission: Permission) -> Self {
        Self::with_anchors(
            permission,
            Arc::new(Mutex::new(AnchorRegistry::new(DEFAULT_FILE_CAP))),
        )
    }

    pub fn with_anchors(permission: Permission, anchors: SharedAnchors) -> Self {
        let description = format!("{DESCRIPTION}{}", permission.scope_note());
        Self {
            permission,
            anchors,
            description,
        }
    }
}

#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct WriteFileArgs {
    #[schemars(description = "要写入的文件路径，相对于工作区根目录；缺失的父目录会自动创建。")]
    pub path: String,

    #[schemars(description = "要写入的完整内容；会覆盖文件原有内容。")]
    pub content: String,
}

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        // schema 是静态的，序列化失败属于编程错误。
        serde_json::to_value(schema_for!(WriteFileArgs))
            .expect("failed to serialize WriteFileArgs schema")
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: WriteFileArgs = serde_json::from_str(args_json).map_err(|e| {
            anyhow::anyhow!("[{}] Failed to deserialize arguments: {e}", self.name())
        })?;

        let target = self.permission.resolve_for_write(&args.path)?;

        let existing = tokio::fs::metadata(&target).await;
        if matches!(&existing, Ok(metadata) if metadata.is_dir()) {
            anyhow::bail!("[{}] `{}` 是目录，不能写入", self.name(), args.path);
        }
        let existed = existing.is_ok();

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("创建父目录失败: {}", parent.display()))?;
        }

        tokio::fs::write(&target, args.content.as_bytes())
            .await
            .with_context(|| format!("写入失败: {}", target.display()))?;
        self.anchors.lock().unwrap().invalidate(&target);

        let action = if existed { "已覆盖" } else { "已新建" };
        Ok(format!(
            "{action} {}（{} 字节）",
            self.permission.display(&target),
            args.content.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::local::test_support::TempDir;

    fn tool_for(dir: &TempDir) -> WriteFile {
        let permission = Permission::workspace(dir.path()).expect("构造工作区失败");
        WriteFile::new(permission)
    }

    #[tokio::test]
    async fn creates_new_file() {
        let dir = TempDir::new();

        let output = tool_for(&dir)
            .execute(r#"{"path":"a.txt","content":"hello"}"#)
            .await
            .expect("应成功");

        assert!(output.contains("已新建"), "输出 = {output}");
        assert!(output.contains("5 字节"), "输出 = {output}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "old content").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"a.txt","content":"hi"}"#)
            .await
            .expect("应成功");

        assert!(output.contains("已覆盖"), "输出 = {output}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let dir = TempDir::new();

        tool_for(&dir)
            .execute(r#"{"path":"a/b/c.txt","content":"deep"}"#)
            .await
            .expect("应成功");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "deep"
        );
    }

    #[tokio::test]
    async fn empty_content_is_allowed() {
        let dir = TempDir::new();

        tool_for(&dir)
            .execute(r#"{"path":"empty.txt","content":""}"#)
            .await
            .expect("应成功");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("empty.txt")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn rejects_directory_target() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join("sub")).expect("建目录失败");

        let error = tool_for(&dir)
            .execute(r#"{"path":"sub","content":"x"}"#)
            .await
            .expect_err("目录目标应报错");

        assert!(format!("{error:#}").contains("是目录"), "错误 = {error:#}");
    }

    #[tokio::test]
    async fn rejects_escaping_path() {
        let dir = TempDir::new();

        assert!(
            tool_for(&dir)
                .execute(r#"{"path":"../outside.txt","content":"x"}"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_malformed_arguments() {
        let dir = TempDir::new();
        assert!(tool_for(&dir).execute("not json").await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writes_through_symlink_inside_root() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        std::fs::write(dir.path().join("real.txt"), "old").expect("写文件失败");
        symlink(dir.path().join("real.txt"), dir.path().join("alias.txt")).expect("建符号链接失败");

        tool_for(&dir)
            .execute(r#"{"path":"alias.txt","content":"new"}"#)
            .await
            .expect("应成功");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("real.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn full_permission_writes_outside_workspace() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let tool = WriteFile::new(Permission::full(dir.path()).expect("构造 full 权限失败"));

        let target = outside.path().join("out.txt");
        let args = format!(r#"{{"path":"{}","content":"escaped"}}"#, target.display());
        let output = tool.execute(&args).await.expect("应成功");

        assert!(output.contains("已新建"), "输出 = {output}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("读取失败"),
            "escaped",
            "文件应真的落在工作区之外"
        );
    }
}
