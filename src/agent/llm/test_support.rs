//! 仅测试 / 离线示例使用的脚本化后端。
//!
//! 单测与示例不该真的打网络，但编排层只认唯一的 [`LLMClient`](super::models::LLMClient)，
//! 所以把「预置回复队列 + 请求记录」做成它内部的一个后端：
//! [`LLMClient::scripted`](super::models::LLMClient::scripted) 挂上它即可离线驱动整条链路。

use std::collections::VecDeque;
use std::sync::Mutex;

use async_openai::types::chat::ChatCompletionRequestMessage;

use super::models::{Reply, ToolPolicy};
use crate::tools::ToolHashMap;

/// 一次脚本化请求的快照：测试据此断言「实际发出去的是什么」。
#[derive(Clone, Debug)]
pub struct ScriptedRequest {
    /// 实际发给传输层的消息序列（已过回调链）。
    pub messages: Vec<ChatCompletionRequestMessage>,
    /// 本次请求暴露的工具名（排序后）。
    pub tool_names: Vec<String>,
    /// 本次请求的 `tool_choice` 策略。
    pub policy: ToolPolicy,
}

/// 脚本化后端：按预置队列返回回复，并记录每次请求。
pub struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<ScriptedRequest>>,
}

impl Scripted {
    pub fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// 记录本次请求，再把队首回复交给调用方；队列空 = 脚本没编排到这一步，直接报错。
    pub(crate) fn next(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&ToolHashMap>,
        policy: &ToolPolicy,
        on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> anyhow::Result<Reply> {
        let mut names: Vec<String> = tools
            .map(|tools| tools.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        self.requests
            .lock()
            .expect("锁被毒化")
            .push(ScriptedRequest {
                messages: messages.to_vec(),
                tool_names: names,
                policy: policy.clone(),
            });

        let reply = self
            .replies
            .lock()
            .expect("锁被毒化")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("预置回复已用尽"))?;
        if !reply.content.is_empty() {
            on_token(&reply.content);
        }
        Ok(reply)
    }

    /// 每次请求的快照，按发生顺序。
    pub fn requests(&self) -> Vec<ScriptedRequest> {
        self.requests.lock().expect("锁被毒化").clone()
    }
}
