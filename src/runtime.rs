//! 顶层 `Agent` 组件：组装一个 `SessionManager`，并把用户 API 委派给它。
//!
//! `Agent` 自己不跑循环——多轮、挂起、恢复都在
//! [`SessionManager`](crate::session::manager::SessionManager) 里，
//! 因为「每个 session 一个常驻 `ReactLoop`」这件事由它持有；
//! 交互式 REPL 循环在 [`Repl`](crate::repl::Repl)，`Agent` 只组装配置、委派会话操作。

use std::sync::Arc;

use crate::llm::models::LLMClient;
use crate::react::approval::{Confirmer, Decision};
use crate::react::models::{Outcome, PendingApproval, Step};
use crate::session::manager::{InMemorySessionManager, SessionManager, SessionRuntimeConfig};
use crate::session::models::{Session, SessionSummary};
use crate::settings::ApprovalPolicy;
use crate::tools::ToolHashMap;

pub struct Agent {
    sessions: Arc<dyn SessionManager>,
    default_user_id: Option<String>,
}

impl Agent {
    /// 组装期入口：模型、工具、system prompt、轮次上限都从这里进。
    pub fn builder(
        llm: Arc<LLMClient>,
        tools: ToolHashMap,
        system_prompt: &str,
        max_turns: usize,
    ) -> AgentBuilder {
        AgentBuilder {
            llm,
            tools,
            system_prompt: system_prompt.to_owned(),
            max_turns,
            approval_policy: ApprovalPolicy::default(),
            confirmer: None,
            default_user_id: None,
        }
    }

    /// 拿到下层的会话管理器（想直接用 trait 方法时用）。
    pub fn sessions(&self) -> &Arc<dyn SessionManager> {
        &self.sessions
    }

    /// 新建会话，`user_id` 取 [`AgentBuilder::default_user`]。
    pub async fn new_session(&self) -> anyhow::Result<Session> {
        self.sessions.create(self.default_user_id.clone()).await
    }

    /// 列出会话摘要；设过 `default_user` 时只列该用户的。
    pub async fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        self.sessions.list(self.default_user_id.as_deref()).await
    }

    pub async fn delete(&self, session_id: &str) -> anyhow::Result<bool> {
        self.sessions.delete(session_id).await
    }

    pub async fn get(&self, session_id: &str) -> anyhow::Result<Option<Session>> {
        self.sessions.get(session_id).await
    }

    /// 取待审内容（非挂起态返回 `None`）。
    pub async fn pending(&self, session_id: &str) -> anyhow::Result<Option<PendingApproval>> {
        self.sessions.pending(session_id).await
    }

    /// 多轮：追加一条 user 消息并跑一轮。
    pub async fn send(
        &self,
        session_id: &str,
        prompt: &str,
        on_step: &mut (dyn for<'x> FnMut(&'x Step) + Send),
        on_token: &mut (dyn for<'x> FnMut(usize, &'x str) + Send),
    ) -> anyhow::Result<Outcome> {
        self.sessions
            .send(session_id, prompt, on_step, on_token)
            .await
    }

    /// 恢复挂起的会话：带上对挂起调用的决定，从中断处继续。
    pub async fn resume(
        &self,
        session_id: &str,
        decision: Decision,
        on_step: &mut (dyn for<'x> FnMut(&'x Step) + Send),
        on_token: &mut (dyn for<'x> FnMut(usize, &'x str) + Send),
    ) -> anyhow::Result<Outcome> {
        self.sessions
            .resume(session_id, decision, on_step, on_token)
            .await
    }
}

/// 组装中的配置。`in_memory()` 是终点。
pub struct AgentBuilder {
    llm: Arc<LLMClient>,
    tools: ToolHashMap,
    system_prompt: String,
    max_turns: usize,
    approval_policy: ApprovalPolicy,
    confirmer: Option<Arc<dyn Confirmer>>,
    default_user_id: Option<String>,
}

impl AgentBuilder {
    pub fn approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        self.approval_policy = policy;
        self
    }

    pub fn confirmer(mut self, confirmer: Arc<dyn Confirmer>) -> Self {
        self.confirmer = Some(confirmer);
        self
    }

    /// 之后 `new_session()` / `list()` 默认使用这个用户标识。
    pub fn default_user(mut self, user_id: impl Into<String>) -> Self {
        self.default_user_id = Some(user_id.into());
        self
    }

    /// 本轮唯一可用的后端；v2 在这里加 `.file_store(dir)`。
    pub fn in_memory(self) -> Agent {
        let config = SessionRuntimeConfig {
            llm: self.llm,
            tools: self.tools,
            system_prompt: self.system_prompt,
            max_turns: self.max_turns,
            approval_policy: self.approval_policy,
            confirmer: self.confirmer,
        };
        Agent {
            sessions: Arc::new(InMemorySessionManager::new(config)),
            default_user_id: self.default_user_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, FunctionCall,
    };
    use serde_json::json;

    use super::*;
    use crate::llm::models::Reply;
    use crate::react::models::DEFAULT_MAX_TURNS;
    use crate::tools::local::final_answer::{FINAL_ANSWER_TOOL, FinalAnswer};
    use crate::tools::tool::Tool;

    /// 脚本化 `LLMClient` 的构造助手（真实类型只有 `LLMClient` 一个）。
    fn scripted(replies: Vec<Reply>) -> Arc<LLMClient> {
        Arc::new(LLMClient::scripted(replies))
    }

    fn agent() -> Agent {
        let mut tools = ToolHashMap::new();
        tools.insert(
            FINAL_ANSWER_TOOL.to_owned(),
            Arc::new(FinalAnswer) as Arc<dyn Tool>,
        );
        Agent::builder(
            scripted(vec![final_reply("完成")]),
            tools,
            "你是测试助手。",
            DEFAULT_MAX_TURNS,
        )
        .in_memory()
    }

    async fn send(agent: &Agent, id: &str, prompt: &str) -> anyhow::Result<Outcome> {
        let mut on_step = |_step: &Step| {};
        let mut on_token = |_turn, _token: &str| {};
        agent.send(id, prompt, &mut on_step, &mut on_token).await
    }

    fn final_reply(text: &str) -> Reply {
        Reply {
            content: String::new(),
            tool_calls: vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: "call_final".to_owned(),
                    function: FunctionCall {
                        name: FINAL_ANSWER_TOOL.to_owned(),
                        arguments: json!({ "answer": text }).to_string(),
                    },
                },
            )],
        }
    }

    #[tokio::test]
    async fn default_user_lands_on_created_sessions() {
        let mut tools = ToolHashMap::new();
        tools.insert(
            FINAL_ANSWER_TOOL.to_owned(),
            Arc::new(FinalAnswer) as Arc<dyn Tool>,
        );
        let agent = Agent::builder(
            scripted(vec![final_reply("完成")]),
            tools,
            "sys",
            DEFAULT_MAX_TURNS,
        )
        .default_user("u1")
        .in_memory();

        let session = agent.new_session().await.expect("create 失败");

        assert_eq!(session.user_id.as_deref(), Some("u1"));
        assert_eq!(agent.list().await.expect("list 失败").len(), 1);
        assert!(session.state.is_empty(), "组装默认值不引入额外状态");
    }

    #[tokio::test]
    async fn builder_defaults_to_allow_and_no_confirmer() {
        let agent = agent();
        let session = agent.new_session().await.expect("create 失败");

        assert!(session.user_id.is_none(), "没设 default_user 就是 None");
        // 默认策略全放行、无 confirmer 时也能正常收尾（若策略要问就会挂起）。
        let outcome = send(&agent, &session.session_id, "随便问问")
            .await
            .expect("send 失败");
        assert_eq!(
            outcome.termination,
            crate::react::models::Termination::FinalAnswer
        );
    }

    #[tokio::test]
    async fn delegation_matches_the_underlying_manager() {
        let agent = agent();
        let session = agent.new_session().await.expect("create 失败");
        let id = session.session_id.clone();

        assert!(agent.get(&id).await.expect("get 失败").is_some());
        assert!(agent.pending(&id).await.expect("pending 失败").is_none());

        let outcome = send(&agent, &id, "提问").await.expect("send 失败");
        assert_eq!(outcome.answer, "完成");

        let listed = agent.list().await.expect("list 失败");
        assert_eq!(listed[0].session_id, id);
        assert_eq!(listed[0].title, "提问");

        assert!(agent.delete(&id).await.expect("delete 失败"));
        assert!(agent.get(&id).await.expect("get 失败").is_none());
    }
}
