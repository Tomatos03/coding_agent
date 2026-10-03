//! 仅测试使用的辅助设施。
//!
//! 单独放这里是为了让文件工具的测试都能拿到一个「必定唯一、Drop 时自动清理」的临时目录，
//! 从而不必为此引入 `tempfile` 之类的额外依赖。

use std::path::{Path, PathBuf};

/// 进程临时目录下的一个唯一子目录，析构时递归删除。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("coding_agent_fs_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("创建临时目录失败");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 清理失败不应让测试 panic（可能已被测试自身删除）。
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
