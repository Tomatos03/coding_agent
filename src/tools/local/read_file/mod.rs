//! 读取工作区内 UTF-8 文本文件的工具。
//!
//! 只读、无副作用。输出带行号，便于模型引用具体行；大文件可用 `offset`/`limit` 分页，
//! 并且总输出有字节上限，避免单个巨型文件撑爆上下文。
//!
//! `line_anchors=true` 时每行改成 `ANCHOR│内容`，并把展示过的行登记进
//! [`crate::tools::local::anchor_registry`] 的服务记录，供 `edit_file` 做行级冲突检测。

use std::sync::{Arc, Mutex};

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::local::anchor_registry::{AnchorRegistry, DEFAULT_FILE_CAP, SharedAnchors};
use crate::tools::local::permission::Permission;
use crate::tools::tool::Tool;
use crate::utils::anchor::ANCHOR_SEP;
use crate::utils::hash::line_fingerprint;

/// 默认返回的行数。
pub const DEFAULT_LIMIT: usize = 2000;
/// 单次最多返回的行数。
pub const MAX_LIMIT: usize = 5000;
/// 单次输出的字节上限；超出的行会被截断并附提示。
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;

const DESCRIPTION: &str = "读取工作区内 UTF-8 文本文件，返回带行号的内容；\
     设 line_anchors=true 则每行带 4 字母锚点（可传给 edit_file 做锚点定位与冲突检测）；\
     可用 offset/limit 分页。";

pub struct ReadFile {
    permission: Permission,
    anchors: SharedAnchors,
    description: String,
}

impl ReadFile {
    /// 独立使用：自带一份私有账本（调用方不需要跨工具共享）。
    pub fn new(permission: Permission) -> Self {
        Self::with_anchors(
            permission,
            Arc::new(Mutex::new(AnchorRegistry::new(DEFAULT_FILE_CAP))),
        )
    }

    /// 与 `edit_file` 等工具共享同一份账本。
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
pub struct ReadFileArgs {
    #[schemars(description = "要读取的文件路径，相对于工作区根目录。")]
    pub path: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1), description = "起始行号，从 1 开始；默认 1。")]
    pub offset: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1), description = "最多返回的行数，默认 2000，上限 5000。")]
    pub limit: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "是否在每行前附带 4 字母行锚点，格式为 `ANCHOR│内容`。默认 false。")]
    pub line_anchors: Option<bool>,
}

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        // schema 是静态的，序列化失败属于编程错误。
        serde_json::to_value(schema_for!(ReadFileArgs))
            .expect("failed to serialize ReadFileArgs schema")
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: ReadFileArgs = serde_json::from_str(args_json).map_err(|e| {
            anyhow::anyhow!("[{}] Failed to deserialize arguments: {e}", self.name())
        })?;

        let path = self.permission.resolve_existing(&args.path)?;
        if tokio::fs::metadata(&path).await?.is_dir() {
            anyhow::bail!("[{}] `{}` 是目录，请用 list_files", self.name(), args.path);
        }

        let bytes = tokio::fs::read(&path).await?;
        if bytes.contains(&0) {
            anyhow::bail!(
                "[{}] `{}` 疑似二进制文件（含 NUL 字节，共 {} 字节），不予读取",
                self.name(),
                args.path,
                bytes.len()
            );
        }
        let content = String::from_utf8(bytes).map_err(|error| {
            let len = error.as_bytes().len();
            anyhow::anyhow!(
                "[{}] `{}` 不是 UTF-8 文本（共 {len} 字节）",
                self.name(),
                args.path
            )
        })?;

        if content.is_empty() {
            return Ok("（文件为空）".to_owned());
        }

        let offset = args.offset.unwrap_or(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        if offset == 0 {
            anyhow::bail!("[{}] offset 从 1 开始", self.name());
        }
        if limit == 0 {
            anyhow::bail!("[{}] limit 必须大于 0", self.name());
        }
        let limit = limit.min(MAX_LIMIT);

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if offset > total {
            return Ok(format!("（offset {offset} 超过文件总行数 {total}）"));
        }

        // 只有显式要求锚点时才计算，避免无谓开销。
        let anchor_data: Option<(Vec<String>, Vec<u64>)> = if args.line_anchors.unwrap_or(false) {
            let fingerprints: Vec<u64> = lines.iter().map(|line| line_fingerprint(line)).collect();
            let anchors = self.anchors.lock().unwrap().align(&path, &fingerprints);
            Some((anchors, fingerprints))
        } else {
            None
        };

        let start = offset - 1;
        let end = (start + limit).min(total);
        let width = total.to_string().len().max(3);

        let mut output = String::new();
        let mut shown = 0usize;

        for (index, line) in lines[start..end].iter().enumerate() {
            let prefix = match &anchor_data {
                Some((anchors, _)) => format!("{}{ANCHOR_SEP}", anchors[start + index]),
                None => format!("{:>width$} | ", offset + index),
            };
            let used = output.len() + prefix.len();
            if used >= MAX_OUTPUT_BYTES {
                break;
            }

            let body = clip_to_bytes(line, MAX_OUTPUT_BYTES - used);
            let clipped = body.len() < line.len();
            output.push_str(&prefix);
            output.push_str(body);
            output.push('\n');
            shown += 1;

            if clipped {
                break;
            }
        }

        if output.ends_with('\n') {
            output.pop();
        }

        if end < total || shown < end - start {
            let last = offset + shown.saturating_sub(1);
            if shown < end - start {
                // 输出被字节上限截断，比单纯翻页更需要注意。
                output.push_str(&format!(
                    "\n…（已截断：文件共 {total} 行，本次显示第 {offset}-{last} 行；用 offset/limit 继续读取）"
                ));
            } else {
                output.push_str(&format!(
                    "\n…（文件共 {total} 行，本次显示第 {offset}-{last} 行；用 offset/limit 继续读取）"
                ));
            }
        }

        if let Some((anchors, fingerprints)) = &anchor_data {
            let shown_indices: Vec<usize> = (start..start + shown).collect();
            if !shown_indices.is_empty() {
                self.anchors.lock().unwrap().record_served(
                    &path,
                    anchors,
                    fingerprints,
                    &shown_indices,
                );
            }
        }

        Ok(output)
    }
}

/// 把 `text` 截到不超过 `max` 字节，并保证落在 UTF-8 字符边界上。
fn clip_to_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }

    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::local::test_support::TempDir;

    fn tool_for(dir: &TempDir) -> ReadFile {
        let permission = Permission::workspace(dir.path()).expect("构造工作区失败");
        ReadFile::new(permission)
    }

    #[tokio::test]
    async fn renders_line_numbers() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"a.txt"}"#)
            .await
            .expect("应成功");

        assert_eq!(output, "  1 | a\n  2 | b\n  3 | c");
    }

    #[tokio::test]
    async fn renders_line_anchors_when_requested() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\na\n").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"a.txt","line_anchors":true}"#)
            .await
            .expect("应成功");

        let rows: Vec<&str> = output.lines().collect();
        assert_eq!(rows.len(), 2, "输出 = {output}");
        let (a0, c0) = rows[0].split_once(ANCHOR_SEP).expect("应有分隔符");
        let (a1, c1) = rows[1].split_once(ANCHOR_SEP).expect("应有分隔符");
        assert_eq!((c0, c1), ("a", "a"));
        assert_eq!(a0.len(), 4);
        assert_eq!(a1.len(), 4);
        assert!(a0.chars().all(|c| c.is_ascii_alphabetic()));
        assert_ne!(a0, a1, "重复行应拿到不同锚点");
    }

    #[tokio::test]
    async fn honours_offset_to_eof() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"a.txt","offset":2}"#)
            .await
            .expect("应成功");

        assert_eq!(output, "  2 | b\n  3 | c");
    }

    #[tokio::test]
    async fn honours_limit_and_reports_more_lines() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"a.txt","limit":1}"#)
            .await
            .expect("应成功");

        assert!(output.starts_with("  1 | a"), "输出 = {output}");
        assert!(
            output.contains("文件共 3 行，本次显示第 1-1 行"),
            "输出 = {output}"
        );
    }

    #[tokio::test]
    async fn empty_file_has_placeholder() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("empty.txt"), "").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"empty.txt"}"#)
            .await
            .expect("应成功");

        assert_eq!(output, "（文件为空）");
    }

    #[tokio::test]
    async fn offset_beyond_eof_is_reported() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"a.txt","offset":5}"#)
            .await
            .expect("应成功");

        assert_eq!(output, "（offset 5 超过文件总行数 2）");
    }

    #[tokio::test]
    async fn rejects_zero_offset_and_limit() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\n").expect("写文件失败");
        let tool = tool_for(&dir);

        assert!(
            tool.execute(r#"{"path":"a.txt","offset":0}"#)
                .await
                .is_err()
        );
        assert!(tool.execute(r#"{"path":"a.txt","limit":0}"#).await.is_err());
    }

    #[tokio::test]
    async fn rejects_non_utf8_and_nul_bytes() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("bad.bin"), [0xff, 0xfe]).expect("写文件失败");
        std::fs::write(dir.path().join("nul.bin"), [b'a', 0, b'b']).expect("写文件失败");
        let tool = tool_for(&dir);

        assert!(tool.execute(r#"{"path":"bad.bin"}"#).await.is_err());
        assert!(tool.execute(r#"{"path":"nul.bin"}"#).await.is_err());
    }

    #[tokio::test]
    async fn rejects_directory_and_escaping_paths() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join("sub")).expect("建目录失败");
        let tool = tool_for(&dir);

        assert!(tool.execute(r#"{"path":"sub"}"#).await.is_err());
        assert!(tool.execute(r#"{"path":"../outside"}"#).await.is_err());
        assert!(tool.execute(r#"{"path":"missing.txt"}"#).await.is_err());
    }

    #[tokio::test]
    async fn truncates_oversized_output() {
        let dir = TempDir::new();
        let line = "x".repeat(200);
        let content = std::iter::repeat_n(line, 3000)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("big.txt"), content).expect("写文件失败");

        let output = tool_for(&dir)
            .execute(r#"{"path":"big.txt","limit":5000}"#)
            .await
            .expect("应成功");

        assert!(output.contains("已截断"), "应提示截断");
        assert!(output.lines().count() < 3000, "不应返回全部行");
    }

    #[tokio::test]
    async fn full_permission_reads_outside_workspace() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("x.txt"), "hello").expect("写文件失败");
        let tool = ReadFile::new(Permission::full(dir.path()).expect("构造 full 权限失败"));

        let target = outside.path().join("x.txt");
        let args = format!(r#"{{"path":"{}"}}"#, target.display());
        let output = tool.execute(&args).await.expect("应成功");

        assert!(output.contains("hello"), "输出 = {output}");
    }

    #[test]
    fn description_reflects_permission_mode() {
        let dir = TempDir::new();

        let workspace = ReadFile::new(Permission::workspace(dir.path()).expect("构造失败"));
        assert_eq!(
            workspace.description(),
            "读取工作区内 UTF-8 文本文件，返回带行号的内容；\
             设 line_anchors=true 则每行带 4 字母锚点（可传给 edit_file 做锚点定位与冲突检测）；\
             可用 offset/limit 分页。"
        );

        let full = ReadFile::new(Permission::full(dir.path()).expect("构造失败"));
        assert!(
            full.description()
                .starts_with("读取工作区内 UTF-8 文本文件")
        );
        assert!(
            full.description().contains("不受工作区边界限制"),
            "描述 = {}",
            full.description()
        );
    }
}
