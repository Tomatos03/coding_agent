//! 文件工具的路径权限：把「能操作哪些路径」从工具实现里抽成可切换的策略。
//!
//! 两种模式：
//! - [`Permission::Workspace`]：限定在工作区根目录内，行为 = [`Workspace`]。当前生产
//!   路径固定使用该模式（[`Permission::current_dir`]），模式选择暂时写死。
//! - [`Permission::Full`]：不做任何边界检查；相对路径仍基于基准目录解析。保留供后续启用。
//!
//! 所有文件工具都必须通过本类型的四个方法与路径打交道，工具自身不做路径判断——
//! 「边界在哪」只有一个实现点，新增工具不会漏掉。

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::tools::local::workspace::{
    Workspace, canonical_dir, path_from_base, resolve_delete_target,
};

/// full 模式追加在工具描述末尾的说明。
const FULL_SCOPE_NOTE: &str = "（当前不受工作区边界限制，可操作任意路径。）";

/// 文件工具的路径权限。
#[derive(Debug, Clone)]
pub enum Permission {
    /// 限定在工作区根目录内（默认）。行为 = 现有 [`Workspace`]，一字不改。
    Workspace(Workspace),
    /// 不做任何边界检查；相对路径基于 `base`（启动时 canonicalize 后固定）。
    Full { base: PathBuf },
}

impl Permission {
    /// 限定工作区；root 须已存在且为目录（校验与 [`Workspace::new`] 相同）。
    pub fn workspace(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        Ok(Self::Workspace(Workspace::new(root)?))
    }

    /// 无边界模式；base 同样须已存在且为目录。
    pub fn full(base: impl AsRef<Path>) -> anyhow::Result<Self> {
        Ok(Self::Full {
            base: canonical_dir(base.as_ref())?,
        })
    }

    /// 权限固定为 workspace 模式；基准目录 = 进程当前目录（agent 的运行目录）。
    pub fn current_dir() -> anyhow::Result<Self> {
        Ok(Self::Workspace(Workspace::current_dir()?))
    }

    /// 两种模式共有的基准目录（workspace 的 root / full 的 base）。
    pub fn base(&self) -> &Path {
        match self {
            Self::Workspace(workspace) => workspace.root(),
            Self::Full { base } => base,
        }
    }

    /// 描述后缀：workspace → `""`（保持原文）；full → 说明不受工作区边界限制。
    pub fn scope_note(&self) -> &'static str {
        match self {
            Self::Workspace(_) => "",
            Self::Full { .. } => FULL_SCOPE_NOTE,
        }
    }

    /// 解析一个**必须已存在**的路径。
    pub fn resolve_existing(&self, user_path: &str) -> anyhow::Result<PathBuf> {
        match self {
            Self::Workspace(workspace) => workspace.resolve_existing(user_path),
            Self::Full { base } => {
                let candidate = path_from_base(base, user_path)?;
                std::fs::canonicalize(&candidate)
                    .with_context(|| format!("路径不存在或不可访问: `{user_path}`"))
            }
        }
    }

    /// 解析一个**允许尚不存在**的写入目标。
    ///
    /// full 模式：目标已存在 → 取真实路径（与 [`Permission::resolve_existing`] 同键，
    /// 锚点账本的失效判定不失真）；不存在 → 原样返回，交给操作系统在真正读写时解析。
    pub fn resolve_for_write(&self, user_path: &str) -> anyhow::Result<PathBuf> {
        match self {
            Self::Workspace(workspace) => workspace.resolve_for_write(user_path),
            Self::Full { base } => {
                let candidate = path_from_base(base, user_path)?;
                match std::fs::canonicalize(&candidate) {
                    Ok(resolved) => Ok(resolved),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
                    Err(error) => {
                        Err(anyhow::Error::new(error)
                            .context(format!("访问路径 `{user_path}` 失败")))
                    }
                }
            }
        }
    }

    /// 解析一个用于**删除**的路径：与 `rm` 一致，不跟随最后一段符号链接。
    pub fn resolve_for_delete(&self, user_path: &str) -> anyhow::Result<PathBuf> {
        match self {
            Self::Workspace(workspace) => workspace.resolve_for_delete(user_path),
            Self::Full { base } => {
                let candidate = path_from_base(base, user_path)?;
                let (resolved, _) = resolve_delete_target(&candidate, user_path)?;
                Ok(resolved)
            }
        }
    }

    /// 展示用路径：能相对基准目录就显示相对路径，否则显示绝对路径。
    pub fn display(&self, path: &Path) -> String {
        match self {
            Self::Workspace(workspace) => workspace.display(path),
            Self::Full { base } => path
                .strip_prefix(base)
                .unwrap_or(path)
                .display()
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::local::test_support::TempDir;

    fn workspace_permission(dir: &TempDir) -> Permission {
        Permission::workspace(dir.path()).expect("构造工作区权限失败")
    }

    fn full_permission(dir: &TempDir) -> Permission {
        Permission::full(dir.path()).expect("构造 full 权限失败")
    }

    #[test]
    fn current_dir_is_workspace_mode_at_process_cwd() {
        let permission = Permission::current_dir().expect("应构造成功");
        let expected = std::fs::canonicalize(std::env::current_dir().expect("应能取到当前目录"))
            .expect("应能 canonicalize");

        assert!(matches!(permission, Permission::Workspace(_)));
        assert_eq!(permission.base(), expected);
    }

    #[test]
    fn scope_note_only_in_full_mode() {
        let dir = TempDir::new();

        assert_eq!(workspace_permission(&dir).scope_note(), "");
        assert!(
            full_permission(&dir)
                .scope_note()
                .contains("不受工作区边界限制"),
            "note = {}",
            full_permission(&dir).scope_note()
        );
    }

    #[test]
    fn workspace_mode_still_rejects_escaping_paths() {
        let dir = TempDir::new();
        let permission = workspace_permission(&dir);

        assert!(permission.resolve_existing("../outside").is_err());
        assert!(permission.resolve_for_write("../outside.txt").is_err());
        assert!(permission.resolve_for_delete("../outside.txt").is_err());
    }

    #[test]
    fn full_resolve_existing_allows_paths_outside_base() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("x.txt"), "x").expect("写文件失败");
        let permission = full_permission(&dir);

        let absolute = outside.path().join("x.txt");
        let resolved = permission
            .resolve_existing(absolute.to_str().expect("路径应为 UTF-8"))
            .expect("基准目录外的绝对路径应放行");
        assert_eq!(
            resolved,
            std::fs::canonicalize(&absolute).expect("应能 canonicalize")
        );
    }

    #[test]
    fn full_resolve_existing_allows_parent_dir() {
        let dir = TempDir::new();
        let permission = full_permission(&dir);
        let name = dir.path().file_name().expect("临时目录应有名字");

        let relative = format!("../{}", name.to_str().expect("名字应为 UTF-8"));
        let resolved = permission.resolve_existing(&relative).expect("`..` 应放行");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path()).expect("应能 canonicalize")
        );
    }

    #[test]
    fn full_resolve_for_write_passes_missing_targets_through() {
        let dir = TempDir::new();
        let canonical = std::fs::canonicalize(dir.path()).expect("应能 canonicalize");
        let permission = full_permission(&dir);

        let resolved = permission
            .resolve_for_write("a/b/c.txt")
            .expect("不存在的嵌套目标应可解析");
        assert_eq!(resolved, canonical.join("a/b/c.txt"));
        assert!(!resolved.exists(), "解析不应创建任何东西");

        let escaped = permission
            .resolve_for_write("../outside.txt")
            .expect("`..` 应放行");
        assert!(
            escaped.ends_with("outside.txt"),
            "解析 = {}",
            escaped.display()
        );
        assert!(!escaped.exists());
    }

    #[test]
    fn full_resolve_for_write_canonicalizes_existing_targets() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hi").expect("写文件失败");
        let permission = full_permission(&dir);

        let resolved = permission.resolve_for_write("a.txt").expect("应能解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path().join("a.txt")).expect("应能 canonicalize"),
            "已存在目标应与 resolve_existing 同键"
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_resolve_for_write_passes_dangling_symlink_through() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        symlink(dir.path().join("missing"), dir.path().join("dangling")).expect("建符号链接失败");
        let permission = full_permission(&dir);

        let resolved = permission
            .resolve_for_write("dangling")
            .expect("悬空链接应放行（写入时由 OS 创建目标）");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path())
                .expect("应能 canonicalize")
                .join("dangling")
        );
    }

    #[test]
    fn full_resolve_for_delete_allows_targets_outside_base() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("x.txt"), "x").expect("写文件失败");
        let permission = full_permission(&dir);

        let absolute = outside.path().join("x.txt");
        let resolved = permission
            .resolve_for_delete(absolute.to_str().expect("路径应为 UTF-8"))
            .expect("基准目录外的目标应可删除");
        assert_eq!(
            resolved,
            std::fs::canonicalize(outside.path())
                .expect("应能 canonicalize")
                .join("x.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_resolve_for_delete_allows_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        symlink(dir.path().join("missing"), dir.path().join("dangling")).expect("建符号链接失败");
        let permission = full_permission(&dir);

        let resolved = permission.resolve_for_delete("dangling").expect("应能解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path())
                .expect("应能 canonicalize")
                .join("dangling"),
            "删的应是链接本身，不是它的目标"
        );
    }

    #[test]
    fn full_display_prefers_relative_path() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");
        let permission = full_permission(&dir);

        let inside = std::fs::canonicalize(dir.path().join("a.txt")).expect("应能 canonicalize");
        assert_eq!(permission.display(&inside), "a.txt");

        let outside_dir = std::fs::canonicalize(outside.path()).expect("应能 canonicalize");
        assert_eq!(
            permission.display(&outside_dir),
            outside_dir.display().to_string()
        );
    }
}
