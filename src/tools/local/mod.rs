//! 本地（进程内）工具集合：每个工具一个子目录，与 [`crate::tools::mcp`] 的远端工具区分。
//!
//! 新增本地工具时，在 `local/` 下创建子目录，并在本文件声明 `pub mod <name>;`。

pub mod web_search;

pub use web_search::WebSearch;
