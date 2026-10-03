//! 编辑工作区内已存在文本文件的工具。
//!
//! 一个工具、三种命令，用 `command` 字段区分（与 Anthropic 的
//! `str_replace_based_edit_tool` 同一思路）：
//!
//! - `str_replace`：精确字符串替换，默认要求 `old_string` 在文件中唯一；
//!   设 `replace_all = true` 才允许替换全部出现位置。
//! - `insert`：在指定行之后插入文本，按文件自身的换行符拼接。
//! - `replace_anchor`：用 `read_file`（`line_anchors=true`）给出的 4 字母行锚点定位，
//!   整段替换若干行。锚点由 [`crate::tools::local::anchor_registry`] 分配；
//!   编辑前对范围内每一行做**行指纹**比对，内容变了的行拒绝（`[E_RANGE_STALE]`），
//!   并把当前范围与新锚点回传，重试无需重新 read。
//!
//! 写回用「同目录临时文件 + rename」做原子替换，避免中途失败把原文件截断；
//! 同时把临时文件的权限对齐原文件，尽量保留可执行位等属性。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::local::anchor_registry::{
    AnchorRegistry, DEFAULT_FILE_CAP, EditSpan, SharedAnchors,
};
use crate::tools::local::permission::Permission;
use crate::tools::tool::Tool;
use crate::utils::hash::line_fingerprint;

const DESCRIPTION: &str = "编辑工作区内已存在的文本文件：command=str_replace 精确字符串替换（默认要求唯一匹配），\
     command=insert 在第 N 行后插入文本，\
     command=replace_anchor 用 read_file 的行锚点整段替换（编辑前按行指纹检测冲突）。";

pub struct EditFile {
    permission: Permission,
    anchors: SharedAnchors,
    description: String,
}

impl EditFile {
    /// 独立使用：自带一份私有账本（`str_replace` / `insert` 不需要共享）；
    /// 想用 `replace_anchor`，请改用 [`EditFile::with_anchors`] 与 `read_file` 共享同一份账本。
    pub fn new(permission: Permission) -> Self {
        Self::with_anchors(
            permission,
            Arc::new(Mutex::new(AnchorRegistry::new(DEFAULT_FILE_CAP))),
        )
    }

    /// 与 `read_file` 等工具共享同一份账本。
    pub fn with_anchors(permission: Permission, anchors: SharedAnchors) -> Self {
        let description = format!("{DESCRIPTION}{}", permission.scope_note());
        Self {
            permission,
            anchors,
            description,
        }
    }

    /// 读取待编辑文件，做与 `read_file` 一致的前置校验。
    async fn load(&self, user_path: &str) -> anyhow::Result<(PathBuf, String)> {
        let path = self.permission.resolve_existing(user_path)?;

        if tokio::fs::metadata(&path).await?.is_dir() {
            anyhow::bail!("[{}] `{user_path}` 是目录，不能编辑", self.name());
        }

        let bytes = tokio::fs::read(&path).await?;
        if bytes.contains(&0) {
            anyhow::bail!(
                "[{}] `{user_path}` 疑似二进制文件（含 NUL 字节），不予编辑",
                self.name()
            );
        }
        let content = String::from_utf8(bytes).map_err(|error| {
            let len = error.as_bytes().len();
            anyhow::anyhow!(
                "[{}] `{user_path}` 不是 UTF-8 文本（共 {len} 字节）",
                self.name()
            )
        })?;

        Ok((path, content))
    }

    /// 编辑成功后让账本跟上新内容：只对已登记的文件做前后缀对齐，
    /// 未变的行保留锚点；内容变化的行换锚点且其 served 记录被清除。
    fn refresh_after_write(&self, path: &Path, content: &str) {
        let fingerprints: Vec<u64> = content.lines().map(line_fingerprint).collect();
        let mut registry = self.anchors.lock().unwrap();
        if registry.contains(path) {
            registry.align(path, &fingerprints);
        }
    }

    async fn str_replace(
        &self,
        user_path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> anyhow::Result<String> {
        if old_string.is_empty() {
            anyhow::bail!("[{}] `old_string` 不能为空", self.name());
        }

        let (path, content) = self.load(user_path).await?;

        let count = content.matches(old_string).count();
        if count == 0 {
            anyhow::bail!(
                "[{}] 在 `{user_path}` 中未找到 `old_string`；请先 read_file 确认原文与空白/缩进",
                self.name()
            );
        }
        if count > 1 && !replace_all {
            anyhow::bail!(
                "[{}] `old_string` 在 `{user_path}` 中出现 {count} 次，不唯一；\
                 请提供更长的上下文，或设置 replace_all=true",
                self.name()
            );
        }

        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        write_atomic(&path, &updated)
            .await
            .with_context(|| format!("写回 `{user_path}` 失败"))?;
        self.refresh_after_write(&path, &updated);

        Ok(format!(
            "已在 {} 替换 {count} 处（文件现共 {} 字节）",
            self.permission.display(&path),
            updated.len()
        ))
    }

    async fn insert(
        &self,
        user_path: &str,
        insert_line: usize,
        insert_text: &str,
    ) -> anyhow::Result<String> {
        let inserted: Vec<&str> = insert_text.lines().collect();
        if inserted.is_empty() {
            anyhow::bail!("[{}] `insert_text` 不能为空", self.name());
        }

        let (path, content) = self.load(user_path).await?;

        // 用文件自身的换行符拼接，避免把 CRLF 文件改成 LF。
        let eol = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let ends_with_newline = content.ends_with('\n');
        let mut lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        if insert_line > total {
            anyhow::bail!(
                "[{}] `insert_line` {insert_line} 超过 `{user_path}` 的总行数 {total}",
                self.name()
            );
        }

        let inserted_count = inserted.len();
        lines.splice(insert_line..insert_line, inserted);

        let mut updated = lines.join(eol);
        if ends_with_newline {
            updated.push_str(eol);
        }

        write_atomic(&path, &updated)
            .await
            .with_context(|| format!("写回 `{user_path}` 失败"))?;
        self.refresh_after_write(&path, &updated);

        Ok(format!(
            "已在 {} 第 {insert_line} 行后插入 {inserted_count} 行（文件现共 {} 行）",
            self.permission.display(&path),
            updated.lines().count()
        ))
    }

    /// 用锚点定位一段行并整体替换；锚点来自 `read_file` 的 `line_anchors=true`。
    async fn replace_anchor(
        &self,
        user_path: &str,
        remove_from: &str,
        remove_to: Option<&str>,
        replacement_lines: &[String],
    ) -> anyhow::Result<String> {
        let (path, content) = self.load(user_path).await?;

        let lines: Vec<String> = content.lines().map(str::to_owned).collect();
        let fingerprints: Vec<u64> = lines.iter().map(|line| line_fingerprint(line)).collect();

        // 先对齐（外部改动会在这里体现），同时确认模型确实读过这个文件。
        let anchors = {
            let mut registry = self.anchors.lock().unwrap();
            if !registry.has_served(&path) {
                anyhow::bail!(
                    "[{}] `{user_path}` 没有本会话的读取记录，请先 read_file（line_anchors=true）",
                    self.name()
                );
            }
            registry.align(&path, &fingerprints)
        };

        let from = resolve_anchor(&anchors, remove_from, user_path)?;
        let to = match remove_to {
            Some(anchor) => resolve_anchor(&anchors, anchor, user_path)?,
            None => from,
        };
        // 与参考实现一致：锚点给反了也接受，按实际顺序替换。
        let (from, to) = if from <= to { (from, to) } else { (to, from) };

        // 行级冲突检测：范围内每一行的指纹必须与展示给模型时一致（纯删除只查首尾）。
        let deletion = replacement_lines.is_empty();
        {
            let registry = self.anchors.lock().unwrap();
            if let Err(mismatched) =
                registry.verify_range(&path, &anchors, &fingerprints, from, to, deletion)
            {
                return Err(anyhow::anyhow!(stale_range_message(
                    user_path,
                    &anchors,
                    &lines,
                    from,
                    to,
                    &mismatched,
                )));
            }
        }

        // 用文件自身的换行符拼接，避免把 CRLF 文件改成 LF。
        let eol = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let ends_with_newline = content.ends_with('\n');
        let mut updated_lines = lines.clone();
        let removed = to - from + 1;
        updated_lines.splice(from..=to, replacement_lines.iter().cloned());

        let mut updated = updated_lines.join(eol);
        if ends_with_newline {
            updated.push_str(eol);
        }

        write_atomic(&path, &updated)
            .await
            .with_context(|| format!("写回 `{user_path}` 失败"))?;

        // 就地精确对齐：范围外、内容未变的行锚点保持不变。
        let new_fingerprints: Vec<u64> = updated.lines().map(line_fingerprint).collect();
        {
            let mut registry = self.anchors.lock().unwrap();
            registry.align_with_span(
                &path,
                &new_fingerprints,
                EditSpan {
                    start: from,
                    end: to + 1,
                    replacement: replacement_lines.len(),
                },
            );
        }

        Ok(format!(
            "已按锚点 {remove_from}..{} 替换 {removed} 行（文件现共 {} 行）",
            remove_to.unwrap_or(remove_from),
            updated.lines().count()
        ))
    }
}

#[async_trait::async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        // schema 是静态的，序列化失败属于编程错误。
        serde_json::to_value(schema_for!(EditFileArgs))
            .expect("failed to serialize EditFileArgs schema")
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: EditFileArgs = serde_json::from_str(args_json).map_err(|e| {
            anyhow::anyhow!("[{}] Failed to deserialize arguments: {e}", self.name())
        })?;

        match args {
            EditFileArgs::StrReplace {
                path,
                old_string,
                new_string,
                replace_all,
            } => {
                self.str_replace(
                    &path,
                    &old_string,
                    &new_string,
                    replace_all.unwrap_or(false),
                )
                .await
            }
            EditFileArgs::Insert {
                path,
                insert_line,
                insert_text,
            } => self.insert(&path, insert_line, &insert_text).await,
            EditFileArgs::ReplaceAnchor {
                path,
                remove_from,
                remove_to,
                replacement_lines,
            } => {
                self.replace_anchor(
                    &path,
                    &remove_from,
                    remove_to.as_deref(),
                    &replacement_lines,
                )
                .await
            }
        }
    }
}

#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum EditFileArgs {
    /// 精确字符串替换。
    StrReplace {
        #[schemars(description = "要编辑的文件路径，相对于工作区根目录。")]
        path: String,

        #[schemars(description = "要被替换的原文，不能为空；默认要求在整个文件中唯一。")]
        old_string: String,

        #[schemars(description = "替换后的新内容，可为空字符串（表示删除）。")]
        new_string: String,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(
            description = "是否替换所有出现位置。默认 false；为 false 且 `old_string` 不唯一时报错。"
        )]
        replace_all: Option<bool>,
    },

    /// 在指定行之后插入文本。
    Insert {
        #[schemars(description = "要编辑的文件路径，相对于工作区根目录。")]
        path: String,

        #[schemars(description = "在第几行之后插入；0 表示文件开头，N 表示第 N 行之后。")]
        insert_line: usize,

        #[schemars(description = "要插入的文本；按行拆开后用文件自身的换行符拼接。")]
        insert_text: String,
    },

    /// 用行锚点定位并整段替换（锚点来自 `read_file` 的 `line_anchors=true`）。
    ReplaceAnchor {
        #[schemars(description = "要编辑的文件路径，相对于工作区根目录。")]
        path: String,

        #[schemars(
            description = "要替换的起始行锚点（4 个字母，取自 read_file 输出中 `│` 之前的部分）。"
        )]
        remove_from: String,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(description = "要替换的结束行锚点；省略时只替换 remove_from 这一行。")]
        remove_to: Option<String>,

        #[schemars(description = "替换后的内容，每个元素一行；空数组表示删除这些行。")]
        replacement_lines: Vec<String>,
    },
}

/// 把 4 字母锚点解析成行下标；找不到说明文件在读之后已经变了。
fn resolve_anchor(anchors: &[String], anchor: &str, user_path: &str) -> anyhow::Result<usize> {
    anchors
        .iter()
        .position(|candidate| candidate == anchor)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[edit_file] 锚点 `{anchor}` 在 `{user_path}` 中不存在；\
                 文件可能已变化，请重新 read_file（line_anchors=true）"
            )
        })
}

/// 冲突反馈：给出当前范围（带新锚点），让模型无需重新 read 即可重试。
fn stale_range_message(
    user_path: &str,
    anchors: &[String],
    lines: &[String],
    from: usize,
    to: usize,
    mismatched: &[usize],
) -> String {
    const MAX_SHOWN: usize = 100;

    let changed = mismatched
        .iter()
        .map(|index| (index + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!(
        "[edit_file] [E_RANGE_STALE] `{user_path}` 第 {}-{} 行中有 {} 行已变（第 {changed} 行）。\n当前范围（带新锚点）：\n",
        from + 1,
        to + 1,
        mismatched.len(),
    );

    let shown_end = (from + MAX_SHOWN).min(to + 1);
    for index in from..shown_end {
        message.push_str(&format!(
            "{}│{}\n",
            anchors[index],
            lines.get(index).map(String::as_str).unwrap_or("")
        ));
    }
    if to + 1 > shown_end {
        message.push_str(&format!(
            "…（范围共 {} 行，只显示前 {} 行）\n",
            to - from + 1,
            shown_end - from
        ));
    }
    message.push_str("请用上面的新锚点重试，无需重新 read。");
    message
}

/// 原子写回：先写同目录临时文件，对齐权限后再 rename 覆盖目标。
async fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定文件父目录: {}", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("路径缺少文件名: {}", path.display()))?
        .to_string_lossy()
        .into_owned();

    let temp = parent.join(format!(".{file_name}.edit-{}.tmp", uuid::Uuid::new_v4()));

    tokio::fs::write(&temp, content.as_bytes()).await?;

    if let Ok(metadata) = tokio::fs::metadata(path).await
        && let Err(error) = tokio::fs::set_permissions(&temp, metadata.permissions()).await
    {
        tracing::warn!("对齐临时文件权限失败: {error}");
    }

    if let Err(error) = tokio::fs::rename(&temp, path).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::local::read_file::ReadFile;
    use crate::tools::local::test_support::TempDir;

    fn tool_for(dir: &TempDir) -> EditFile {
        let permission = Permission::workspace(dir.path()).expect("构造工作区失败");
        EditFile::new(permission)
    }

    /// 共享同一份账本的 read + edit，供锚点相关测试使用。
    fn anchored_tools(dir: &TempDir) -> (ReadFile, EditFile) {
        let permission = Permission::workspace(dir.path()).expect("构造工作区失败");
        let anchors: SharedAnchors = Arc::new(Mutex::new(AnchorRegistry::new(16)));
        (
            ReadFile::with_anchors(permission.clone(), anchors.clone()),
            EditFile::with_anchors(permission, anchors),
        )
    }

    fn read(dir: &TempDir, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(name)).expect("读文件失败")
    }

    /// 通过 read_file（line_anchors=true）建立服务记录并取回当前行序锚点。
    async fn read_anchors(reader: &ReadFile, path: &str) -> Vec<String> {
        let output = reader
            .execute(&format!(r#"{{"path":"{path}","line_anchors":true}}"#))
            .await
            .expect("读取应成功");
        output
            .lines()
            .filter_map(|line| line.split_once('│').map(|(anchor, _)| anchor.to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hello world").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(
                r#"{"command":"str_replace","path":"a.txt","old_string":"world","new_string":"rust"}"#,
            )
            .await
            .expect("应成功");

        assert_eq!(read(&dir, "a.txt"), "hello rust");
        assert!(output.contains("替换 1 处"), "输出 = {output}");
    }

    #[tokio::test]
    async fn allows_empty_new_string_for_deletion() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "abc").expect("写文件失败");

        tool_for(&dir)
            .execute(r#"{"command":"str_replace","path":"a.txt","old_string":"b","new_string":""}"#)
            .await
            .expect("应成功");

        assert_eq!(read(&dir, "a.txt"), "ac");
    }

    #[tokio::test]
    async fn rejects_missing_match() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hello").expect("写文件失败");

        let error = tool_for(&dir)
            .execute(
                r#"{"command":"str_replace","path":"a.txt","old_string":"nope","new_string":"x"}"#,
            )
            .await
            .expect_err("未匹配应报错");

        assert!(format!("{error:#}").contains("未找到"), "错误 = {error:#}");
        assert_eq!(read(&dir, "a.txt"), "hello", "失败时不应改动文件");
    }

    #[tokio::test]
    async fn rejects_ambiguous_match_without_replace_all() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "aa xx aa").expect("写文件失败");

        let error = tool_for(&dir)
            .execute(
                r#"{"command":"str_replace","path":"a.txt","old_string":"aa","new_string":"bb"}"#,
            )
            .await
            .expect_err("多处匹配应报错");

        assert!(format!("{error:#}").contains("不唯一"), "错误 = {error:#}");
        assert_eq!(read(&dir, "a.txt"), "aa xx aa");
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "aa xx aa").expect("写文件失败");

        let output = tool_for(&dir)
            .execute(
                r#"{"command":"str_replace","path":"a.txt","old_string":"aa","new_string":"bb","replace_all":true}"#,
            )
            .await
            .expect("应成功");

        assert_eq!(read(&dir, "a.txt"), "bb xx bb");
        assert!(output.contains("替换 2 处"), "输出 = {output}");
    }

    #[tokio::test]
    async fn rejects_empty_old_string() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hello").expect("写文件失败");

        assert!(
            tool_for(&dir)
                .execute(
                    r#"{"command":"str_replace","path":"a.txt","old_string":"","new_string":"x"}"#
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn str_replace_preserves_missing_trailing_newline() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "abc").expect("写文件失败");

        tool_for(&dir)
            .execute(
                r#"{"command":"str_replace","path":"a.txt","old_string":"b","new_string":"X"}"#,
            )
            .await
            .expect("应成功");

        assert_eq!(read(&dir, "a.txt"), "aXc");
    }

    #[tokio::test]
    async fn inserts_at_beginning_middle_and_end() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").expect("写文件失败");
        let tool = tool_for(&dir);

        // 中间
        tool.execute(r#"{"command":"insert","path":"a.txt","insert_line":1,"insert_text":"X"}"#)
            .await
            .expect("应成功");
        assert_eq!(read(&dir, "a.txt"), "a\nX\nb\nc\n");

        // 开头
        tool.execute(r#"{"command":"insert","path":"a.txt","insert_line":0,"insert_text":"head"}"#)
            .await
            .expect("应成功");
        assert_eq!(read(&dir, "a.txt"), "head\na\nX\nb\nc\n");

        // 结尾
        let total = read(&dir, "a.txt").lines().count();
        tool.execute(&format!(
            r#"{{"command":"insert","path":"a.txt","insert_line":{total},"insert_text":"tail"}}"#
        ))
        .await
        .expect("应成功");
        assert_eq!(read(&dir, "a.txt"), "head\na\nX\nb\nc\ntail\n");
    }

    #[tokio::test]
    async fn inserts_multiline_text() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").expect("写文件失败");

        tool_for(&dir)
            .execute(r#"{"command":"insert","path":"a.txt","insert_line":1,"insert_text":"X\nY"}"#)
            .await
            .expect("应成功");

        assert_eq!(read(&dir, "a.txt"), "a\nX\nY\nb\n");
    }

    #[tokio::test]
    async fn inserts_into_empty_and_unterminated_files() {
        let dir = TempDir::new();

        std::fs::write(dir.path().join("empty.txt"), "").expect("写文件失败");
        tool_for(&dir)
            .execute(r#"{"command":"insert","path":"empty.txt","insert_line":0,"insert_text":"X"}"#)
            .await
            .expect("应成功");
        assert_eq!(read(&dir, "empty.txt"), "X");

        std::fs::write(dir.path().join("noeol.txt"), "a\nb").expect("写文件失败");
        tool_for(&dir)
            .execute(r#"{"command":"insert","path":"noeol.txt","insert_line":2,"insert_text":"X"}"#)
            .await
            .expect("应成功");
        assert_eq!(read(&dir, "noeol.txt"), "a\nb\nX");
    }

    #[tokio::test]
    async fn preserves_crlf_when_inserting() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\r\nb\r\n").expect("写文件失败");

        tool_for(&dir)
            .execute(r#"{"command":"insert","path":"a.txt","insert_line":1,"insert_text":"X"}"#)
            .await
            .expect("应成功");

        assert_eq!(read(&dir, "a.txt"), "a\r\nX\r\nb\r\n");
    }

    #[tokio::test]
    async fn rejects_insert_line_beyond_end_and_empty_text() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").expect("写文件失败");
        let tool = tool_for(&dir);

        assert!(
            tool.execute(
                r#"{"command":"insert","path":"a.txt","insert_line":9,"insert_text":"X"}"#
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(r#"{"command":"insert","path":"a.txt","insert_line":0,"insert_text":""}"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_directories_binaries_and_escaping_paths() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.path().join("sub")).expect("建目录失败");
        std::fs::write(dir.path().join("nul.bin"), [b'a', 0, b'b']).expect("写文件失败");
        let tool = tool_for(&dir);

        assert!(
            tool.execute(
                r#"{"command":"str_replace","path":"sub","old_string":"a","new_string":"b"}"#
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(
                r#"{"command":"str_replace","path":"nul.bin","old_string":"a","new_string":"b"}"#
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(
                r#"{"command":"str_replace","path":"../outside","old_string":"a","new_string":"b"}"#
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(
                r#"{"command":"str_replace","path":"missing.txt","old_string":"a","new_string":"b"}"#
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_malformed_arguments() {
        let dir = TempDir::new();
        let tool = tool_for(&dir);

        assert!(tool.execute("not json").await.is_err());
        // 缺少 command 字段
        assert!(
            tool.execute(r#"{"path":"a.txt","old_string":"a","new_string":"b"}"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn leaves_no_temp_files_behind() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "hello").expect("写文件失败");

        tool_for(&dir)
            .execute(
                r#"{"command":"str_replace","path":"a.txt","old_string":"hello","new_string":"world"}"#,
            )
            .await
            .expect("应成功");

        let entries = std::fs::read_dir(dir.path()).expect("读目录失败").count();
        assert_eq!(entries, 1, "只应剩目标文件，不应有临时文件");
    }

    #[tokio::test]
    async fn replace_anchor_replaces_a_line_range() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("写文件失败");
        let (reader, editor) = anchored_tools(&dir);
        let anchors = read_anchors(&reader, "a.txt").await;

        editor
            .execute(&format!(
                r#"{{"command":"replace_anchor","path":"a.txt","remove_from":"{}","remove_to":"{}","replacement_lines":["TWO"]}}"#,
                anchors[1], anchors[2]
            ))
            .await
            .expect("应成功");

        assert_eq!(read(&dir, "a.txt"), "one\nTWO\n");
    }

    #[tokio::test]
    async fn replace_anchor_single_line_then_delete() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("写文件失败");
        let (reader, editor) = anchored_tools(&dir);
        let anchors = read_anchors(&reader, "a.txt").await;

        editor
            .execute(&format!(
                r#"{{"command":"replace_anchor","path":"a.txt","remove_from":"{}","replacement_lines":["2"]}}"#,
                anchors[1]
            ))
            .await
            .expect("应成功");
        assert_eq!(read(&dir, "a.txt"), "one\n2\nthree\n");

        // 空数组表示删除这些行。
        let anchors = read_anchors(&reader, "a.txt").await;
        editor
            .execute(&format!(
                r#"{{"command":"replace_anchor","path":"a.txt","remove_from":"{}","replacement_lines":[]}}"#,
                anchors[1]
            ))
            .await
            .expect("应成功");
        assert_eq!(read(&dir, "a.txt"), "one\nthree\n");
    }

    #[tokio::test]
    async fn replace_anchor_requires_prior_read() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("写文件失败");
        let (_reader, editor) = anchored_tools(&dir);

        let error = editor
            .execute(
                r#"{"command":"replace_anchor","path":"a.txt","remove_from":"Hasu","replacement_lines":["x"]}"#,
            )
            .await
            .expect_err("未读取过应报错");

        assert!(
            format!("{error:#}").contains("读取记录"),
            "错误 = {error:#}"
        );
        assert_eq!(read(&dir, "a.txt"), "one\ntwo\n");
    }

    #[tokio::test]
    async fn replace_anchor_rejects_unknown_anchor() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("写文件失败");
        let (reader, editor) = anchored_tools(&dir);
        let _ = read_anchors(&reader, "a.txt").await;

        let error = editor
            .execute(
                r#"{"command":"replace_anchor","path":"a.txt","remove_from":"ZZZZ","replacement_lines":["x"]}"#,
            )
            .await
            .expect_err("不存在的锚点应报错");

        assert!(format!("{error:#}").contains("不存在"), "错误 = {error:#}");
        assert_eq!(read(&dir, "a.txt"), "one\ntwo\n", "拒绝时不应改动文件");
    }

    #[tokio::test]
    async fn replace_anchor_rejects_when_interior_line_changed() {
        let dir = TempDir::new();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").expect("写文件失败");
        let (reader, editor) = anchored_tools(&dir);
        let anchors = read_anchors(&reader, "a.txt").await;

        // 外部改动中间一行；首尾锚点仍然有效。
        std::fs::write(&path, "one\nTWO\nthree\nfour\n").expect("外部改动失败");

        let error = editor
            .execute(&format!(
                r#"{{"command":"replace_anchor","path":"a.txt","remove_from":"{}","remove_to":"{}","replacement_lines":["X"]}}"#,
                anchors[0], anchors[3]
            ))
            .await
            .expect_err("中间行已变应报冲突");

        assert!(
            format!("{error:#}").contains("E_RANGE_STALE"),
            "错误 = {error:#}"
        );
        assert_eq!(
            read(&dir, "a.txt"),
            "one\nTWO\nthree\nfour\n",
            "拒绝时不应改动文件"
        );
    }

    #[tokio::test]
    async fn replace_anchor_keeps_outside_anchors_stable() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").expect("写文件失败");
        let (reader, editor) = anchored_tools(&dir);
        let before = read_anchors(&reader, "a.txt").await;

        // 用两行替换中间两行。
        editor
            .execute(&format!(
                r#"{{"command":"replace_anchor","path":"a.txt","remove_from":"{}","remove_to":"{}","replacement_lines":["TWO"]}}"#,
                before[1], before[2]
            ))
            .await
            .expect("应成功");
        assert_eq!(read(&dir, "a.txt"), "one\nTWO\nfour\n");

        let after = read_anchors(&reader, "a.txt").await;
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], before[0], "区间前的行锚点不应变化");
        assert_eq!(after[2], before[3], "区间后的行锚点不应变化");
    }

    #[tokio::test]
    async fn replace_anchor_accepts_reversed_range() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("写文件失败");
        let (reader, editor) = anchored_tools(&dir);
        let anchors = read_anchors(&reader, "a.txt").await;

        editor
            .execute(&format!(
                r#"{{"command":"replace_anchor","path":"a.txt","remove_from":"{}","remove_to":"{}","replacement_lines":["X","Y"]}}"#,
                anchors[2], anchors[0]
            ))
            .await
            .expect("锚点给反应按实际顺序替换");

        assert_eq!(read(&dir, "a.txt"), "X\nY\n");
    }

    #[test]
    fn schema_is_command_tagged_union() {
        let schema = serde_json::to_value(schema_for!(EditFileArgs)).expect("schema 应可序列化");
        let text = schema.to_string();

        assert!(
            schema.get("oneOf").is_some(),
            "内部标签枚举应生成 oneOf，实际 schema = {text}"
        );
        assert!(text.contains("str_replace"), "schema = {text}");
        assert!(text.contains("insert"), "schema = {text}");
        assert!(text.contains("replace_anchor"), "schema = {text}");
    }

    #[tokio::test]
    async fn full_permission_edits_outside_workspace() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("x.txt"), "hello world").expect("写文件失败");
        let tool = EditFile::new(Permission::full(dir.path()).expect("构造 full 权限失败"));

        let target = outside.path().join("x.txt");
        let args = format!(
            r#"{{"command":"str_replace","path":"{}","old_string":"world","new_string":"there"}}"#,
            target.display()
        );
        tool.execute(&args).await.expect("应成功");

        assert_eq!(
            std::fs::read_to_string(&target).expect("读取失败"),
            "hello there",
            "文件应真的被改在工作区之外"
        );
    }
}
