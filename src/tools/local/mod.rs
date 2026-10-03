//! 本地（进程内）工具集合：每个工具一个子目录，与 [`crate::tools::mcp`] 的远端工具区分。
//!
//! 新增本地工具时，在 `local/` 下创建子目录，并在本文件声明 `pub mod <name>;`。
//! 文件类工具共享 [`permission`] 里的路径权限（`workspace` 模式的边界逻辑在 [`workspace`]）；
//! `test_support` 只在测试构建中存在。

pub mod anchor_registry;
pub mod delete_files;
pub mod edit_file;
pub mod final_answer;
pub mod list_files;
pub mod permission;
pub mod read_file;
pub mod web_search;
pub mod workspace;
pub mod write_file;

#[cfg(test)]
pub(crate) mod test_support;

pub use anchor_registry::{AnchorRegistry, DEFAULT_FILE_CAP, FileAnchors, SharedAnchors};
pub use delete_files::DeleteFiles;
pub use edit_file::EditFile;
pub use final_answer::{FINAL_ANSWER_TOOL, FinalAnswer};
pub use list_files::ListFiles;
pub use permission::Permission;
pub use read_file::ReadFile;
pub use web_search::WebSearch;
pub use workspace::Workspace;
pub use write_file::WriteFile;
