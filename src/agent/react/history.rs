use async_openai::types::chat::{
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageArgs,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
};

pub struct History {
    messages: Vec<ChatCompletionRequestMessage>,
}

/// 历史里最后一个 assistant 批次中**尚未执行**的调用。
///
/// 它是挂起态的唯一判据：正常结束的 run 绝不会留下未配对的 `tool_call`
/// （不变量②），所以「`pending_batch` 返回 `Some` ⇔ 会话停在半途」。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingBatch {
    /// 该批次的全部 function 调用（已滤掉无法执行、无法配对的变体）。
    pub calls: Vec<ChatCompletionMessageToolCalls>,
    /// 下一个待执行的调用下标：前面 `next` 个已有配对的 tool 消息。
    pub next: usize,
}

/// 从消息序列推导恢复游标（纯函数）。
///
/// 规则：找最后一条带非空 `tool_calls` 的 assistant 消息，数它**紧跟其后**的
/// `tool` 消息条数 `k`；`k` 就是下一个待执行的下标。`k == calls.len()` 说明
/// 批次已跑完，返回 `None`。
pub fn pending_batch(messages: &[ChatCompletionRequestMessage]) -> Option<PendingBatch> {
    let (index, calls) = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            let ChatCompletionRequestMessage::Assistant(assistant) = message else {
                return None;
            };
            let calls: Vec<ChatCompletionMessageToolCalls> = assistant
                .tool_calls
                .as_ref()?
                .iter()
                .filter(|call| matches!(call, ChatCompletionMessageToolCalls::Function(_)))
                .cloned()
                .collect();
            (!calls.is_empty()).then_some((index, calls))
        })?;

    let executed = messages[index + 1..]
        .iter()
        .take_while(|message| matches!(message, ChatCompletionRequestMessage::Tool(_)))
        .count();

    (executed < calls.len()).then_some(PendingBatch {
        calls,
        next: executed,
    })
}

impl History {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    pub fn system(&mut self, content: &str) -> anyhow::Result<()> {
        let message = ChatCompletionRequestSystemMessageArgs::default()
            .content(content)
            .build()?;
        self.messages.push(message.into());
        Ok(())
    }

    pub fn user(&mut self, content: &str) -> anyhow::Result<()> {
        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(content)
            .build()?;
        self.messages.push(message.into());
        Ok(())
    }

    pub fn assistant(
        &mut self,
        content: &str,
        tool_calls: Vec<ChatCompletionMessageToolCalls>,
    ) -> anyhow::Result<()> {
        let mut builder = ChatCompletionRequestAssistantMessageArgs::default();
        if !content.is_empty() {
            builder.content(content);
        }
        if !tool_calls.is_empty() {
            builder.tool_calls(tool_calls);
        }
        self.messages.push(builder.build()?.into());
        Ok(())
    }

    pub fn tool(&mut self, tool_call_id: &str, content: &str) -> anyhow::Result<()> {
        let message = ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(tool_call_id)
            .content(content)
            .build()?;
        self.messages.push(message.into());
        Ok(())
    }

    pub fn as_slice(&self) -> &[ChatCompletionRequestMessage] {
        &self.messages
    }

    /// 直接吃一段既有消息（session 层恢复冷会话时用）。
    ///
    /// 不做结构校验：**历史即事实**——形状由写入方（`ReactLoop`）的不变量②保证，
    /// session 只负责原样搬运。
    pub fn from_messages(messages: Vec<ChatCompletionRequestMessage>) -> Self {
        Self { messages }
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}
