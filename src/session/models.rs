//! Session 的数据模型与派生函数。
//!
//! 设计要点：**不额外存状态字段**。标题、挂起态、消息数全部按需从 `history` 派生，
//! 因而既定的 schema 一字不改；也保证「历史即事实」——只要历史合法，派生结果
//! 就一定自洽。

use std::collections::HashMap;

use async_openai::types::chat::{
    ChatCompletionMessageToolCalls, ChatCompletionRequestMessage,
    ChatCompletionRequestUserMessageContent,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::react::history::pending_batch;

/// 标题截断的最大字符数（不是字节数）。
const TITLE_MAX_CHARS: usize = 32;
/// 会话里还没有用户消息时的占位标题。
const UNTITLED: &str = "(未命名会话)";

/// `state` 里的保留键。其余键归调用方（如 GAIA 进度）。
pub mod state_keys {
    /// 挂起时所在轮次；`resume` 靠它决定从第几轮继续、`max_turns` 预算还剩多少。
    pub const TURN: &str = "turn";
    /// 展示用副本：`{ reason, tool, arguments, tool_call_id, turn }`。
    pub const PAUSE: &str = "pause";
}

/// 一段对话的完整存档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// 唯一标识（uuid v4）。
    pub session_id: String,
    /// 可选用户标识。core 只透传，不做鉴权 / 隔离。
    pub user_id: Option<String>,
    /// 完整对话历史（含 system）。会话的**唯一真相**，冷启动时据此重建 `ReactLoop`。
    pub history: Vec<ChatCompletionRequestMessage>,
    /// 任意执行状态。保留键见 [`state_keys`]。
    pub state: HashMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 挂起判据的轻量视图：历史里那个尚未执行的调用。
///
/// 它只带历史里有的东西；`turn` 与 `description` 由 `SessionManager` 补齐成
/// [`crate::react::models::PendingApproval`]。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCall {
    pub tool_call_id: String,
    pub tool: String,
    pub arguments: String,
}

/// 列表用摘要：不携带完整历史，`list` 不必克隆大对象。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub user_id: Option<String>,
    /// 派生：首条 user 消息截断。
    pub title: String,
    /// 派生：`history.len()`。
    pub message_count: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// 派生自历史：存在未配对的 `tool_call`。
    pub suspended: bool,
    /// 待审批的工具名（挂起时才有），供列表一眼看出在等什么。
    pub pending_tool: Option<String>,
}

impl Session {
    /// 挂起判据（从历史推导）：存在未被 `tool` 消息配对的 `tool_call`。
    ///
    /// 正常结束的 run 绝不会留下这种形状（ReAct 不变量②），所以它是充要条件。
    pub fn pending_call(&self) -> Option<PendingCall> {
        let batch = pending_batch(&self.history)?;
        let ChatCompletionMessageToolCalls::Function(call) = batch.calls.get(batch.next)? else {
            return None;
        };
        Some(PendingCall {
            tool_call_id: call.id.clone(),
            tool: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        })
    }

    /// 挂起时所在轮次（由 `SessionManager` 写入 `state`）。
    pub fn suspended_turn(&self) -> Option<usize> {
        self.state
            .get(state_keys::TURN)?
            .as_u64()
            .map(|turn| turn as usize)
    }

    /// 标题：首条 user 消息截断到 [`TITLE_MAX_CHARS`] 字符；没有则用占位文案。
    pub fn title(&self) -> String {
        for message in &self.history {
            let ChatCompletionRequestMessage::User(user) = message else {
                continue;
            };
            let ChatCompletionRequestUserMessageContent::Text(text) = &user.content else {
                continue;
            };
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return truncate(trimmed, TITLE_MAX_CHARS);
            }
        }
        UNTITLED.to_owned()
    }

    /// 列表摘要：标题 / 消息数 / 挂起态都在这里一次性派生出来。
    pub fn summary(&self) -> SessionSummary {
        let pending = self.pending_call();
        SessionSummary {
            session_id: self.session_id.clone(),
            user_id: self.user_id.clone(),
            title: self.title(),
            message_count: self.history.len(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            suspended: pending.is_some(),
            pending_tool: pending.map(|call| call.tool),
        }
    }
}

/// 按**字符**（不是字节）截断，超长时加省略号。
fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
        ChatCompletionRequestUserMessageArgs, FunctionCall,
    };
    use serde_json::json;

    use super::*;

    fn session(history: Vec<ChatCompletionRequestMessage>) -> Session {
        let now = Utc::now();
        Session {
            session_id: "s1".to_owned(),
            user_id: None,
            history,
            state: HashMap::new(),
            created_at: now,
            updated_at: now,
        }
    }

    fn system(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestSystemMessageArgs::default()
            .content(text)
            .build()
            .expect("构造 system 消息失败")
            .into()
    }

    fn user(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestUserMessageArgs::default()
            .content(text)
            .build()
            .expect("构造 user 消息失败")
            .into()
    }

    fn assistant_with_calls(calls: Vec<(&str, &str, &str)>) -> ChatCompletionRequestMessage {
        let calls: Vec<ChatCompletionMessageToolCalls> = calls
            .into_iter()
            .map(|(id, name, arguments)| {
                ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                    id: id.to_owned(),
                    function: FunctionCall {
                        name: name.to_owned(),
                        arguments: arguments.to_owned(),
                    },
                })
            })
            .collect();
        ChatCompletionRequestAssistantMessageArgs::default()
            .tool_calls(calls)
            .build()
            .expect("构造 assistant 消息失败")
            .into()
    }

    fn tool(tool_call_id: &str, content: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(tool_call_id)
            .content(content)
            .build()
            .expect("构造 tool 消息失败")
            .into()
    }

    #[test]
    fn title_uses_first_user_message_and_truncates_by_chars() {
        assert_eq!(session(vec![system("sys")]).title(), UNTITLED);

        let long = "这是一条很长很长的用户提问".repeat(3);
        let derived = session(vec![system("sys"), user(&long)]).title();
        assert_eq!(
            derived.chars().count(),
            TITLE_MAX_CHARS + 1,
            "截断后带一个省略号"
        );
        assert!(derived.ends_with('…'));
        assert!(long.starts_with(derived.trim_end_matches('…')));

        assert_eq!(
            session(vec![system("sys"), user("  短标题  ")]).title(),
            "短标题"
        );
        // 空白的 user 消息不当作标题。
        assert_eq!(session(vec![user("   ")]).title(), UNTITLED);
    }

    #[test]
    fn pending_call_is_derived_from_history() {
        // 只有 system：没有挂起。
        assert!(session(vec![system("sys")]).pending_call().is_none());

        // 批次全部配对：没有挂起。
        let paired = session(vec![
            assistant_with_calls(vec![("a", "echo", "{}")]),
            tool("a", "ok"),
        ]);
        assert!(paired.pending_call().is_none());

        // 批次里第一个未执行（k = 0）。
        let fresh = session(vec![assistant_with_calls(vec![
            ("a", "echo", "{}"),
            ("b", "read", "{}"),
        ])]);
        assert_eq!(
            fresh.pending_call(),
            Some(PendingCall {
                tool_call_id: "a".to_owned(),
                tool: "echo".to_owned(),
                arguments: "{}".to_owned(),
            })
        );

        // 批次里第二个未执行（k > 0）。
        let partial = session(vec![
            assistant_with_calls(vec![("a", "echo", "{}"), ("b", "read", "{}")]),
            tool("a", "ok"),
        ]);
        assert_eq!(
            partial.pending_call().map(|call| call.tool_call_id),
            Some("b".to_owned())
        );

        // 批次已完成（正常收尾后的历史）。
        assert!(
            session(vec![
                assistant_with_calls(vec![("a", "echo", "{}")]),
                tool("a", "ok")
            ])
            .pending_call()
            .is_none()
        );
    }

    #[test]
    fn summary_reports_suspension_and_counts() {
        let mut value = session(vec![
            system("sys"),
            user("帮我个忙"),
            assistant_with_calls(vec![("a", "echo", "{}")]),
        ]);
        value.state.insert(state_keys::TURN.to_owned(), json!(3));

        let summary = value.summary();
        assert_eq!(summary.title, "帮我个忙");
        assert_eq!(summary.message_count, 3);
        assert!(summary.suspended);
        assert_eq!(summary.pending_tool.as_deref(), Some("echo"));
        assert_eq!(value.suspended_turn(), Some(3));
    }

    #[test]
    fn session_round_trips_through_serde() {
        let mut value = session(vec![system("sys"), user("你好")]);
        value
            .state
            .insert("custom".to_owned(), json!({ "progress": 1 }));

        let json = serde_json::to_string(&value).expect("序列化失败");
        let restored: Session = serde_json::from_str(&json).expect("反序列化失败");

        assert_eq!(restored, value, "为 v2 文件后端钉住形状");
    }
}
