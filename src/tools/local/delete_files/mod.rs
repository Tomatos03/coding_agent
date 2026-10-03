//! 删除工作区内文件或目录的工具。
//!
//! 目录与文件一视同仁：目录会被**递归删除**（`remove_dir_all`）；符号链接只删链接本身、
//! 不跟随目标。删除不可逆，调用方应谨慎。
//!
//! 批量语义分两阶段：
//! 1. 先对 `paths` 里的每一项做整体校验，任何一项不合法就一个都不删，避免删一半；
//! 2. 再逐个删除，成功/失败项都会汇总，部分失败时把结果一并报告。
//!
//! 删除路径与 `rm` 一致：**不跟随最后一段符号链接**，即删的是链接本身而不是它的目标；
//! 但父目录会做真实路径校验，防止经由符号链接删到工作区外。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::local::anchor_registry::{AnchorRegistry, DEFAULT_FILE_CAP, SharedAnchors};
use crate::tools::local::permission::Permission;
use crate::tools::tool::Tool;

const DESCRIPTION: &str = "删除工作区内的一个或多个文件/目录（目录递归删除）；先整体校验再删除。";

pub struct DeleteFiles {
    permission: Permission,
    anchors: SharedAnchors,
    description: String,
}

impl DeleteFiles {
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
pub struct DeleteFilesArgs {
    #[schemars(
        length(min = 1),
        description = "要删除的文件或目录路径列表，相对于工作区根目录；目录会被递归删除。"
    )]
    pub paths: Vec<String>,
}

#[async_trait::async_trait]
impl Tool for DeleteFiles {
    fn name(&self) -> &str {
        "delete_files"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        // schema 是静态的，序列化失败属于编程错误。
        serde_json::to_value(schema_for!(DeleteFilesArgs))
            .expect("failed to serialize DeleteFilesArgs schema")
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: DeleteFilesArgs = serde_json::from_str(args_json).map_err(|e| {
            anyhow::anyhow!("[{}] Failed to deserialize arguments: {e}", self.name())
        })?;

        if args.paths.is_empty() {
            anyhow::bail!("[{}] `paths` 不能为空", self.name());
        }

        // 阶段一：整体校验。任何一项不合法就整体失败，不开始删除。
        let mut targets: Vec<PathBuf> = Vec::with_capacity(args.paths.len());
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for user_path in &args.paths {
            let resolved = self.permission.resolve_for_delete(user_path)?;
            // 同一次调用里的重复路径只删一次。
            if seen.insert(resolved.clone()) {
                targets.push(resolved);
            }
        }

        // 阶段二：逐个删除，收集成功与失败。目录递归删除；符号链接只删链接本身。
        let mut deleted: Vec<String> = Vec::with_capacity(targets.len());
        let mut failed: Vec<String> = Vec::new();
        for target in &targets {
            // `symlink_metadata` 不跟随符号链接：指向目录的链接仍按链接删除。
            let is_dir = match tokio::fs::symlink_metadata(target).await {
                Ok(metadata) => metadata.is_dir(),
                Err(error) => {
                    failed.push(format!("{}（{error}）", self.permission.display(target)));
                    continue;
                }
            };

            let result = if is_dir {
                tokio::fs::remove_dir_all(target).await
            } else {
                tokio::fs::remove_file(target).await
            };

            match result {
                Ok(()) => {
                    self.anchors.lock().unwrap().invalidate(target);
                    let display = self.permission.display(target);
                    deleted.push(if is_dir {
                        format!("{display}/")
                    } else {
                        display
                    });
                }
                Err(error) => {
                    failed.push(format!("{}（{error}）", self.permission.display(target)));
                }
            }
        }

        if failed.is_empty() {
            Ok(format!(
                "已删除 {} 个文件/目录：{}",
                deleted.len(),
                deleted.join(", ")
            ))
        } else {
            anyhow::bail!(
                "[{}] 删除完成但有失败：成功 {} 项，失败 {} 项；失败项：{}",
                self.name(),
                deleted.len(),
                failed.len(),
                failed.join("; ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::local::test_support::TempDir;

    fn tool_for(dir: &TempDir) -> DeleteFiles {
        let permission = Permission::workspace(dir.path()).expect("构造工作区失败");
        DeleteFiles::new(permission)
    }

    fn exists(dir: &TempDir, name: &str) -> bool {
        std::fs::symlink_metadata(dir.path().join(name)).is_ok()
    }

    #[tokio::test]
    async fn deletes_single_file() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"paths":["a.txt"]}"#)
            .await
            .expect("应成功");

        assert!(!exists(&dir, "a.txt"));
        assert!(output.contains("已删除 1 个文件"), "输出 = {output}");
    }

    #[tokio::test]
    async fn deletes_multiple_files() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");
        std::fs::write(dir.path().join("b.txt"), "x").expect("写文件失败");
        std::fs::write(dir.path().join("keep.txt"), "x").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"paths":["a.txt","b.txt"]}"#)
            .await
            .expect("应成功");

        assert!(!exists(&dir, "a.txt"));
        assert!(!exists(&dir, "b.txt"));
        assert!(exists(&dir, "keep.txt"));
        assert!(output.contains("已删除 2 个文件"), "输出 = {output}");
    }

    #[tokio::test]
    async fn deduplicates_repeated_paths() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"paths":["a.txt","a.txt"]}"#)
            .await
            .expect("重复路径不应导致失败");

        assert!(!exists(&dir, "a.txt"));
        assert!(output.contains("已删除 1 个文件"), "输出 = {output}");
    }

    #[tokio::test]
    async fn validates_before_deleting_anything() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");

        let error = tool_for(&dir)
            .execute(r#"{"paths":["a.txt","missing.txt"]}"#)
            .await
            .expect_err("含不存在路径应整体失败");

        assert!(format!("{error:#}").contains("不存在"), "错误 = {error:#}");
        assert!(exists(&dir, "a.txt"), "校验失败时不应删除任何文件");
    }

    #[tokio::test]
    async fn deletes_directory_recursively() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.path().join("sub/nested")).expect("建目录失败");
        std::fs::write(dir.path().join("sub/a.txt"), "x").expect("写文件失败");
        std::fs::write(dir.path().join("sub/nested/b.txt"), "x").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"paths":["sub"]}"#)
            .await
            .expect("应成功");

        assert!(!exists(&dir, "sub"), "目录应被递归删除");
        assert!(output.contains("sub/"), "目录应带 / 标记，输出 = {output}");
    }

    #[tokio::test]
    async fn deletes_empty_directory() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join("empty")).expect("建目录失败");

        tool_for(&dir)
            .execute(r#"{"paths":["empty"]}"#)
            .await
            .expect("应成功");

        assert!(!exists(&dir, "empty"));
    }

    #[tokio::test]
    async fn deletes_files_and_directories_together() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");
        std::fs::create_dir(dir.path().join("sub")).expect("建目录失败");

        let output = tool_for(&dir)
            .execute(r#"{"paths":["a.txt","sub"]}"#)
            .await
            .expect("应成功");

        assert!(!exists(&dir, "a.txt"));
        assert!(!exists(&dir, "sub"));
        assert!(output.contains("已删除 2 个文件/目录"), "输出 = {output}");
    }

    #[tokio::test]
    async fn rejects_missing_and_escaping_paths() {
        let dir = TempDir::new();
        let tool = tool_for(&dir);

        assert!(tool.execute(r#"{"paths":["missing.txt"]}"#).await.is_err());
        assert!(
            tool.execute(r#"{"paths":["../outside.txt"]}"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_empty_and_malformed_arguments() {
        let dir = TempDir::new();
        let tool = tool_for(&dir);

        assert!(tool.execute(r#"{"paths":[]}"#).await.is_err());
        assert!(tool.execute("not json").await.is_err());
        assert!(tool.execute(r#"{}"#).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deletes_symlink_itself_not_its_target() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        std::fs::write(dir.path().join("real.txt"), "keep me").expect("写文件失败");
        symlink(dir.path().join("real.txt"), dir.path().join("link")).expect("建符号链接失败");

        tool_for(&dir)
            .execute(r#"{"paths":["link"]}"#)
            .await
            .expect("应成功");

        assert!(!exists(&dir, "link"), "链接本身应被删除");
        assert!(exists(&dir, "real.txt"), "链接目标不应被删除");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("real.txt")).unwrap(),
            "keep me"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_parent_escaping_via_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("secret.txt"), "s").expect("写文件失败");
        symlink(outside.path(), dir.path().join("link")).expect("建符号链接失败");

        assert!(
            tool_for(&dir)
                .execute(r#"{"paths":["link/secret.txt"]}"#)
                .await
                .is_err()
        );
        assert!(
            outside.path().join("secret.txt").is_file(),
            "区外文件不应被删除"
        );
    }

    #[tokio::test]
    async fn full_permission_deletes_outside_workspace() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("x.txt"), "x").expect("写文件失败");
        let tool = DeleteFiles::new(Permission::full(dir.path()).expect("构造 full 权限失败"));

        let target = outside.path().join("x.txt");
        let args = format!(r#"{{"paths":["{}"]}}"#, target.display());
        let output = tool.execute(&args).await.expect("应成功");

        assert!(output.contains("已删除 1 个文件"), "输出 = {output}");
        assert!(!target.exists(), "区外文件应被删除");
    }
}
