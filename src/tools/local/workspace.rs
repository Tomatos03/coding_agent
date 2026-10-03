//! 文件类本地工具的工作区边界与路径解析（`workspace` 权限模式的核心实现）。
//!
//! 所有文件工具都必须通过 [`crate::tools::local::permission::Permission`] 解析用户给出的
//! 路径，工具自身不做路径判断：这样「边界在哪」只有一个实现点，新增工具不会漏掉。
//! 本类型的全部行为由 [`crate::tools::local::permission::Permission::Workspace`] 变体复用。
//!
//! 有意不做 TOCTOU 防护（校验与实际读写之间的竞态）：本工具运行在单进程 agent 内，
//! 不存在会主动替换路径的对手；真正的隔离应由操作系统/沙箱层提供。

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;

/// 文件工具可访问的根目录边界。
///
/// `root` 在构造时已 canonicalize，因此后续所有比较都是「真实路径」之间的比较，
/// 符号链接无法伪造出前缀相同的假象。
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// 用显式根目录构造；路径不存在或不是目录时返回 `Err`。
    pub fn new(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        Ok(Self {
            root: canonical_dir(root.as_ref())?,
        })
    }

    /// 用进程当前目录构造：工作区根目录总是 agent 的运行目录，不由环境变量控制。
    pub fn current_dir() -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().context("获取当前工作目录失败")?;
        Self::new(cwd)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 解析一个**必须已存在**的路径，并确保其真实路径落在工作区内。
    ///
    /// 路径可以是相对根目录的相对路径，也可以是根目录内的绝对路径。
    pub fn resolve_existing(&self, user_path: &str) -> anyhow::Result<PathBuf> {
        let candidate = self.join(user_path)?;
        let canonical = std::fs::canonicalize(&candidate)
            .with_context(|| format!("路径不存在或不可访问: `{user_path}`"))?;
        self.ensure_within(&canonical, user_path)?;
        Ok(canonical)
    }

    /// 展示用路径：能相对根目录就显示相对路径，否则显示绝对路径。
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// 解析一个**允许尚不存在**的写入目标，并确保其真实位置落在工作区内。
    ///
    /// 与 [`Workspace::resolve_existing`] 的区别：目标不存在时不报错，而是
    /// canonicalize 最近的已存在祖先后，再把剩余组件拼回去；[`Self::join`] 已拒绝
    /// `..`，所以剩余组件只能是普通名字，不可能逃逸。
    ///
    /// 符号链接（含悬空链接）按真实目标校验：指向区外一律拒绝；悬空链接无法
    /// canonicalize，同样拒绝——否则写入会顺着链接落到工作区之外。
    pub fn resolve_for_write(&self, user_path: &str) -> anyhow::Result<PathBuf> {
        let candidate = self.join(user_path)?;
        let mut current: &Path = &candidate;
        let mut suffix: Vec<OsString> = Vec::new();

        loop {
            match std::fs::symlink_metadata(current) {
                Ok(_) => {
                    let canonical = std::fs::canonicalize(current)
                        .with_context(|| format!("路径不可访问: `{user_path}`"))?;
                    self.ensure_within(&canonical, user_path)?;

                    let mut resolved = canonical;
                    for component in suffix.iter().rev() {
                        resolved.push(component);
                    }
                    return Ok(resolved);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let Some(parent) = current.parent() else {
                        anyhow::bail!("路径 `{user_path}` 无法解析");
                    };
                    if let Some(name) = current.file_name() {
                        suffix.push(name.to_os_string());
                    }
                    current = parent;
                }
                Err(error) => {
                    return Err(
                        anyhow::Error::new(error).context(format!("访问路径 `{user_path}` 失败"))
                    );
                }
            }
        }
    }

    /// 解析一个用于**删除**的路径：与 `rm` 一致，**不跟随最后一段符号链接**。
    ///
    /// 只校验父目录的真实位置在工作区内，因此：
    /// - 可以删除悬空符号链接（[`Workspace::resolve_existing`] 做不到）；
    /// - 链接指向区外时删除的是链接本身，不会误删外部文件；
    /// - 父目录若经由符号链接逃出工作区，则拒绝。
    pub fn resolve_for_delete(&self, user_path: &str) -> anyhow::Result<PathBuf> {
        let candidate = self.join(user_path)?;
        let (resolved, canonical_parent) = resolve_delete_target(&candidate, user_path)?;
        self.ensure_within(&canonical_parent, user_path)?;
        Ok(resolved)
    }

    fn join(&self, user_path: &str) -> anyhow::Result<PathBuf> {
        let candidate = path_from_base(&self.root, user_path)?;

        if candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            anyhow::bail!("路径 `{user_path}` 不允许包含 `..`");
        }

        Ok(candidate)
    }

    fn ensure_within(&self, canonical: &Path, user_path: &str) -> anyhow::Result<()> {
        if canonical.starts_with(&self.root) {
            Ok(())
        } else {
            anyhow::bail!("路径 `{user_path}` 超出工作区范围: {}", self.root.display())
        }
    }
}

/// 目录的「存在 + 是目录」校验：canonicalize 后返回真实路径。
pub(crate) fn canonical_dir(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("目录不存在或不可访问: {}", path.display()))?;

    if !canonical.is_dir() {
        anyhow::bail!("路径不是目录: {}", canonical.display());
    }

    Ok(canonical)
}

/// 把用户路径拼到基准目录上：空串拒绝；绝对路径原样；相对路径 `base.join`。
///
/// 不检查 `..`——是否允许由调用方（权限模式）决定。
pub(crate) fn path_from_base(base: &Path, user_path: &str) -> anyhow::Result<PathBuf> {
    if user_path.trim().is_empty() {
        anyhow::bail!("路径不能为空");
    }

    let path = Path::new(user_path);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

/// 删除类解析的公共骨架（调用方已完成空串与 `..` 检查）。
///
/// 与 `rm` 一致：目标必须存在（`symlink_metadata` 不跟随符号链接，悬空链接也算存在），
/// 但最后一段符号链接只删链接本身；父目录 canonicalize 后与文件名拼回。
/// 返回 `(解析结果, 父目录的真实路径)`，后者供边界校验使用。
pub(crate) fn resolve_delete_target(
    candidate: &Path,
    user_path: &str,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    std::fs::symlink_metadata(candidate).with_context(|| format!("路径不存在: `{user_path}`"))?;

    let parent = candidate
        .parent()
        .ok_or_else(|| anyhow::anyhow!("路径 `{user_path}` 无法解析"))?;
    let canonical_parent =
        std::fs::canonicalize(parent).with_context(|| format!("父目录不可访问: `{user_path}`"))?;

    let file_name = candidate
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("路径 `{user_path}` 缺少文件名"))?;

    Ok((canonical_parent.join(file_name), canonical_parent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::local::test_support::TempDir;

    #[test]
    fn new_accepts_existing_directory() {
        let dir = TempDir::new();
        let workspace = Workspace::new(dir.path()).expect("目录应可作为根");

        assert_eq!(
            workspace.root(),
            std::fs::canonicalize(dir.path()).expect("应能 canonicalize")
        );
    }

    #[test]
    fn new_rejects_missing_path() {
        let dir = TempDir::new();
        assert!(Workspace::new(dir.path().join("does-not-exist")).is_err());
    }

    #[test]
    fn new_rejects_file() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("f.txt"), "x").expect("写文件失败");
        assert!(Workspace::new(dir.path().join("f.txt")).is_err());
    }

    #[test]
    fn resolves_relative_path_inside_root() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hi").expect("写文件失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        let resolved = workspace.resolve_existing("a.txt").expect("应能解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path().join("a.txt")).expect("应能 canonicalize")
        );
    }

    #[test]
    fn resolves_absolute_path_inside_root() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hi").expect("写文件失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        let absolute = dir.path().join("a.txt");
        let resolved = workspace
            .resolve_existing(absolute.to_str().expect("路径应为 UTF-8"))
            .expect("根内绝对路径应能解析");
        assert!(resolved.starts_with(workspace.root()));
    }

    #[test]
    fn rejects_parent_dir_component() {
        let dir = TempDir::new();
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        assert!(workspace.resolve_existing("../outside").is_err());
    }

    #[test]
    fn rejects_empty_path() {
        let dir = TempDir::new();
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        assert!(workspace.resolve_existing("").is_err());
        assert!(workspace.resolve_existing("   ").is_err());
    }

    #[test]
    fn rejects_path_outside_root() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("x.txt"), "x").expect("写文件失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        let outside_file = outside.path().join("x.txt");
        assert!(
            workspace
                .resolve_existing(outside_file.to_str().expect("路径应为 UTF-8"))
                .is_err()
        );
    }

    #[test]
    fn display_prefers_relative_path() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        let file = std::fs::canonicalize(dir.path().join("a.txt")).expect("应能 canonicalize");
        assert_eq!(workspace.display(&file), "a.txt");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_root() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("secret.txt"), "s").expect("写文件失败");
        symlink(outside.path(), dir.path().join("link")).expect("建符号链接失败");

        let workspace = Workspace::new(dir.path()).expect("构造失败");
        assert!(workspace.resolve_existing("link/secret.txt").is_err());
    }

    #[test]
    fn write_target_may_not_exist_yet() {
        let dir = TempDir::new();
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        let resolved = workspace
            .resolve_for_write("a/b/c.txt")
            .expect("不存在的嵌套目标应可解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path())
                .expect("应能 canonicalize")
                .join("a/b/c.txt")
        );
        assert!(!resolved.exists(), "解析不应创建任何东西");
    }

    #[test]
    fn write_target_resolves_existing_file() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hi").expect("写文件失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        let resolved = workspace.resolve_for_write("a.txt").expect("应能解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path().join("a.txt")).expect("应能 canonicalize")
        );
    }

    #[test]
    fn write_target_rejects_parent_dir_and_outside_root() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        assert!(workspace.resolve_for_write("../outside.txt").is_err());
        assert!(
            workspace
                .resolve_for_write(
                    outside
                        .path()
                        .join("x.txt")
                        .to_str()
                        .expect("路径应为 UTF-8")
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_target_rejects_escaping_and_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("secret.txt"), "s").expect("写文件失败");
        symlink(outside.path(), dir.path().join("link")).expect("建符号链接失败");
        symlink(dir.path().join("missing"), dir.path().join("dangling")).expect("建符号链接失败");

        let workspace = Workspace::new(dir.path()).expect("构造失败");
        assert!(workspace.resolve_for_write("link/secret.txt").is_err());
        assert!(workspace.resolve_for_write("dangling").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn write_target_follows_symlink_inside_root() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        std::fs::write(dir.path().join("real.txt"), "hi").expect("写文件失败");
        symlink(dir.path().join("real.txt"), dir.path().join("alias.txt")).expect("建符号链接失败");

        let workspace = Workspace::new(dir.path()).expect("构造失败");
        let resolved = workspace.resolve_for_write("alias.txt").expect("应能解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path().join("real.txt")).expect("应能 canonicalize")
        );
    }

    #[test]
    fn delete_target_resolves_regular_file() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        let resolved = workspace.resolve_for_delete("a.txt").expect("应能解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path())
                .expect("应能 canonicalize")
                .join("a.txt")
        );
    }

    #[test]
    fn delete_target_rejects_missing_parent_dir_and_root() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        assert!(workspace.resolve_for_delete("missing.txt").is_err());
        assert!(workspace.resolve_for_delete("../outside.txt").is_err());
        assert!(workspace.resolve_for_delete("..").is_err());
        assert!(
            workspace
                .resolve_for_delete(
                    outside
                        .path()
                        .join("x.txt")
                        .to_str()
                        .expect("路径应为 UTF-8")
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_target_allows_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        symlink(dir.path().join("missing"), dir.path().join("dangling")).expect("建符号链接失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        // 与 rm 一致：解析出链接本身，而不是它的（不存在的）目标。
        let resolved = workspace.resolve_for_delete("dangling").expect("应能解析");
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path())
                .expect("应能 canonicalize")
                .join("dangling")
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_target_rejects_parent_escaping_via_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("secret.txt"), "s").expect("写文件失败");
        symlink(outside.path(), dir.path().join("link")).expect("建符号链接失败");
        let workspace = Workspace::new(dir.path()).expect("构造失败");

        assert!(workspace.resolve_for_delete("link/secret.txt").is_err());
    }
}
