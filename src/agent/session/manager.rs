//! 会话运行时：`SessionManager` trait 与它的内存实现。
//!
//! 每个 session 对应一个**长期存活、支持多次提问**的 [`ReactLoop`]
//! （[`SessionEntry`] 把会话数据与它的 loop 放在一起）。`session.history` 是 run
//! 收尾后回写的**快照**；冷启动或换后端时用 [`ReactLoop::from_history`] 反向重建。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::agent::llm::models::LLMClient;
use crate::agent::react::approval::{ApprovalRequest, Confirmer, Decision};
use crate::agent::react::history::History;
use crate::agent::react::models::{Outcome, PendingApproval, Step, Termination};
use crate::agent::react::runner::ReactLoop;
use crate::settings::ApprovalPolicy;
use crate::tools::ToolHashMap;

use super::models::{Session, SessionSummary, state_keys};

/// 建一个 session 的运行时所需的全部配置。manager 用它为每个新会话建 loop。
#[derive(Clone)]
pub struct SessionRuntimeConfig {
    pub llm: Arc<LLMClient>,
    pub tools: ToolHashMap,
    pub system_prompt: String,
    pub max_turns: usize,
    pub approval_policy: ApprovalPolicy,
    pub confirmer: Option<Arc<dyn Confirmer>>,
}

/// 会话数据 + 它的活体 loop。
struct SessionEntry {
    session: Session,
    engine: ReactLoop,
}

/// 会话注册表：每个 session 一个常驻 `ReactLoop`，并负责挂起标记与恢复游标。
#[async_trait]
pub trait SessionManager: Send + Sync {
    /// 新建会话：history 已含 system，loop 同时就绪；返回完整对象（含 id）。
    async fn create(&self, user_id: Option<String>) -> anyhow::Result<Session>;

    /// 不存在时返回 `Ok(None)`（「没找到」不是错误）。返回的是**快照**。
    async fn get(&self, session_id: &str) -> anyhow::Result<Option<Session>>;

    /// 按 `updated_at` 降序；`user_id` 为 `Some` 时只列该用户的。
    async fn list(&self, user_id: Option<&str>) -> anyhow::Result<Vec<SessionSummary>>;

    /// 返回是否真的删掉了（连同它的 loop 一起丢弃）。
    async fn delete(&self, session_id: &str) -> anyhow::Result<bool>;

    /// 多轮：追加一条 user 消息并跑一轮。会话处于挂起态时返回 `Err`。
    async fn send(
        &self,
        session_id: &str,
        prompt: &str,
        on_step: &mut (dyn for<'x> FnMut(&'x Step) + Send),
        on_token: &mut (dyn for<'x> FnMut(usize, &'x str) + Send),
    ) -> anyhow::Result<Outcome>;

    /// 恢复：带上对挂起调用的决定，从中断处继续。非挂起态返回 `Err`。
    async fn resume(
        &self,
        session_id: &str,
        decision: Decision,
        on_step: &mut (dyn for<'x> FnMut(&'x Step) + Send),
        on_token: &mut (dyn for<'x> FnMut(usize, &'x str) + Send),
    ) -> anyhow::Result<Outcome>;

    /// 取待审内容（非挂起态返回 `None`）。挂起可无限期，这是「回来批」的入口。
    async fn pending(&self, session_id: &str) -> anyhow::Result<Option<PendingApproval>>;
}

pub struct InMemorySessionManager {
    config: SessionRuntimeConfig,
    /// 外层 `RwLock` 只做查表 / 插入 / 删除：临界区极短、绝不跨 await；
    /// 每个会话一把 `tokio::sync::Mutex`，在整轮 run 期间持有。
    sessions: RwLock<HashMap<String, Arc<Mutex<SessionEntry>>>>,
}

impl InMemorySessionManager {
    pub fn new(config: SessionRuntimeConfig) -> Self {
        Self {
            config,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// 查表拿句柄（不判断存在性）。
    fn lookup(&self, session_id: &str) -> Option<Arc<Mutex<SessionEntry>>> {
        self.sessions
            .read()
            .expect("会话表锁被毒化")
            .get(session_id)
            .cloned()
    }

    /// 同 [`Self::lookup`]，但不存在时直接报错——用于 `send` / `resume` / `pending`
    /// 这类「id 错了就是调用方 bug」的操作。
    fn handle(&self, session_id: &str) -> anyhow::Result<Arc<Mutex<SessionEntry>>> {
        self.lookup(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在：{session_id}"))
    }

    /// 用一段历史造一个 loop（`create` 与将来的冷启动恢复共用）。
    fn engine_for(&self, history: History) -> anyhow::Result<ReactLoop> {
        let mut engine = ReactLoop::from_history(
            history,
            self.config.llm.clone(),
            self.config.tools.clone(),
            self.config.max_turns,
        )?
        .with_approval_policy(self.config.approval_policy.clone());
        if let Some(confirmer) = &self.config.confirmer {
            engine = engine.with_confirmer(confirmer.clone());
        }
        Ok(engine)
    }

    /// run 收尾：回写快照、按结果维护 `state` 与 `updated_at`。
    ///
    /// 挂起时写 `turn` / `pause`（后者是给人看的副本，真相仍在历史里）；
    /// 正常结束时清掉它们。loop 不销毁——会话继续挂着，随时可 `resume`。
    fn checkpoint(entry: &mut SessionEntry, outcome: &Outcome) {
        entry.session.history = entry.engine.history().to_vec();
        entry.session.updated_at = Utc::now();

        match &outcome.termination {
            Termination::Suspended => {
                let pending = outcome
                    .pending
                    .as_ref()
                    .expect("Suspended 必然携带 pending");
                entry
                    .session
                    .state
                    .insert(state_keys::TURN.to_owned(), json!(outcome.turns));
                entry.session.state.insert(
                    state_keys::PAUSE.to_owned(),
                    json!({
                        "reason": "approval",
                        "tool": pending.request.tool,
                        "arguments": pending.request.arguments,
                        "tool_call_id": pending.tool_call_id,
                        "turn": outcome.turns,
                    }),
                );
            }
            _ => {
                entry.session.state.remove(state_keys::PAUSE);
                entry.session.state.remove(state_keys::TURN);
            }
        }
    }

    /// 把「历史里的待决调用」补齐成完整的 [`PendingApproval`]：
    /// `turn` 来自 `state`，`description` 来自工具表。
    fn pending_of(&self, session: &Session) -> Option<PendingApproval> {
        let call = session.pending_call()?;
        let turn = session.suspended_turn().unwrap_or(1);
        Some(PendingApproval {
            tool_call_id: call.tool_call_id,
            request: ApprovalRequest {
                turn,
                tool: call.tool.clone(),
                description: self
                    .config
                    .tools
                    .get(&call.tool)
                    .map(|tool| tool.description().to_owned())
                    .unwrap_or_default(),
                arguments: call.arguments,
            },
        })
    }
}

#[async_trait]
impl SessionManager for InMemorySessionManager {
    async fn create(&self, user_id: Option<String>) -> anyhow::Result<Session> {
        let mut history = History::new();
        history.system(&self.config.system_prompt)?;
        let engine = self.engine_for(history)?;

        let now = Utc::now();
        let session = Session {
            session_id: Uuid::new_v4().to_string(),
            user_id,
            history: engine.history().to_vec(),
            state: HashMap::new(),
            created_at: now,
            updated_at: now,
        };

        self.sessions.write().expect("会话表锁被毒化").insert(
            session.session_id.clone(),
            Arc::new(Mutex::new(SessionEntry {
                session: session.clone(),
                engine,
            })),
        );

        Ok(session)
    }

    async fn get(&self, session_id: &str) -> anyhow::Result<Option<Session>> {
        let Some(handle) = self.lookup(session_id) else {
            return Ok(None);
        };
        let entry = handle.lock().await;
        Ok(Some(entry.session.clone()))
    }

    async fn list(&self, user_id: Option<&str>) -> anyhow::Result<Vec<SessionSummary>> {
        // 先把句柄收集出来、释放外层锁，再逐个 await 内层锁。
        let handles: Vec<Arc<Mutex<SessionEntry>>> = self
            .sessions
            .read()
            .expect("会话表锁被毒化")
            .values()
            .cloned()
            .collect();

        let mut summaries = Vec::with_capacity(handles.len());
        for handle in handles {
            let entry = handle.lock().await;
            let session = &entry.session;
            if user_id.is_some_and(|user| session.user_id.as_deref() != Some(user)) {
                continue;
            }
            summaries.push(session.summary());
        }

        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
        Ok(summaries)
    }

    async fn delete(&self, session_id: &str) -> anyhow::Result<bool> {
        Ok(self
            .sessions
            .write()
            .expect("会话表锁被毒化")
            .remove(session_id)
            .is_some())
    }

    async fn send(
        &self,
        session_id: &str,
        prompt: &str,
        on_step: &mut (dyn for<'x> FnMut(&'x Step) + Send),
        on_token: &mut (dyn for<'x> FnMut(usize, &'x str) + Send),
    ) -> anyhow::Result<Outcome> {
        let handle = self.handle(session_id)?;
        let mut entry = handle.lock().await;

        if let Some(pending) = entry.session.pending_call() {
            anyhow::bail!(
                "会话处于挂起状态（待审工具 `{}`），请先 resume",
                pending.tool
            );
        }

        let outcome = entry.engine.run(prompt, on_step, on_token).await?;

        Self::checkpoint(&mut entry, &outcome);
        Ok(outcome)
    }

    async fn resume(
        &self,
        session_id: &str,
        decision: Decision,
        on_step: &mut (dyn for<'x> FnMut(&'x Step) + Send),
        on_token: &mut (dyn for<'x> FnMut(usize, &'x str) + Send),
    ) -> anyhow::Result<Outcome> {
        let handle = self.handle(session_id)?;
        let mut entry = handle.lock().await;

        let Some(pending) = entry.session.pending_call() else {
            anyhow::bail!("会话不处于挂起状态：没有未配对的 tool_call");
        };
        let turn = entry.session.suspended_turn().unwrap_or(1);
        tracing::info!(tool = %pending.tool, turn, "恢复挂起的会话");

        let outcome = entry
            .engine
            .resume(decision, turn, on_step, on_token)
            .await?;

        Self::checkpoint(&mut entry, &outcome);
        Ok(outcome)
    }

    async fn pending(&self, session_id: &str) -> anyhow::Result<Option<PendingApproval>> {
        let handle = self.handle(session_id)?;
        let entry = handle.lock().await;
        Ok(self.pending_of(&entry.session))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestMessage, FunctionCall,
    };
    use serde_json::Value;
    use tokio::sync::Notify;

    use super::*;
    use crate::agent::react::models::DEFAULT_MAX_TURNS;
    use crate::settings::{ApprovalAction, ApprovalRule};
    use crate::tools::local::final_answer::{FINAL_ANSWER_TOOL, FinalAnswer};
    use crate::tools::tool::Tool;

    use crate::agent::llm::callback::{Callback, CallbackEvent};
    use crate::agent::llm::models::Reply;

    /// 脚本化 `LLMClient`：按预置回复队列驱动会话，并记录每次请求。
    fn scripted(replies: Vec<Reply>) -> Arc<LLMClient> {
        Arc::new(LLMClient::scripted(replies))
    }

    /// 每次请求实际收到的消息（已过回调链）。
    fn seen(llm: &LLMClient) -> Vec<Vec<ChatCompletionRequestMessage>> {
        llm.scripted_requests()
            .into_iter()
            .map(|request| request.messages)
            .collect()
    }

    /// `BeforeSend` 命中标记就把请求挂住，直到被放行——用来证明「不同 session 互不阻塞」。
    struct Gate {
        marker: String,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Callback for Gate {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            let CallbackEvent::BeforeSend { messages } = event else {
                return Ok(());
            };
            let hit = messages.iter().any(|message| {
                serde_json::to_string(message)
                    .unwrap_or_default()
                    .contains(&self.marker)
            });
            if hit {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(())
        }
    }

    /// 探针工具：记录执行次数，返回固定前缀 + 参数。
    struct ProbeTool {
        name: String,
        executed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "探针工具"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
            self.executed.fetch_add(1, Ordering::Relaxed);
            Ok(format!("probe:{args_json}"))
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> Reply {
        Reply {
            content: String::new(),
            tool_calls: vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: id.to_owned(),
                    function: FunctionCall {
                        name: name.to_owned(),
                        arguments: arguments.to_owned(),
                    },
                },
            )],
        }
    }

    fn final_answer(text: &str) -> Reply {
        call(
            "call_final",
            FINAL_ANSWER_TOOL,
            &json!({ "answer": text }).to_string(),
        )
    }

    /// 只对匹配 `pattern` 的工具判 `ask`，其余放行。
    fn ask_for(pattern: &str) -> ApprovalPolicy {
        ApprovalPolicy {
            rules: vec![ApprovalRule {
                pattern: pattern.to_owned(),
                action: ApprovalAction::Ask,
            }],
            ..Default::default()
        }
    }

    struct Harness {
        manager: Arc<InMemorySessionManager>,
        executed: Arc<AtomicUsize>,
    }

    fn harness(llm: Arc<LLMClient>, policy: ApprovalPolicy) -> Harness {
        let executed = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolHashMap::new();
        tools.insert(
            "probe".to_owned(),
            Arc::new(ProbeTool {
                name: "probe".to_owned(),
                executed: executed.clone(),
            }) as Arc<dyn Tool>,
        );
        tools.insert(
            FINAL_ANSWER_TOOL.to_owned(),
            Arc::new(FinalAnswer) as Arc<dyn Tool>,
        );

        let config = SessionRuntimeConfig {
            llm,
            tools,
            system_prompt: "你是测试助手。".to_owned(),
            max_turns: DEFAULT_MAX_TURNS,
            approval_policy: policy,
            confirmer: None,
        };
        Harness {
            manager: Arc::new(InMemorySessionManager::new(config)),
            executed,
        }
    }

    /// 空操作的两个回调（形式参数，测试不消费流）。
    fn noop_step() -> impl FnMut(&Step) + Send {
        |_step: &Step| {}
    }

    fn noop_token() -> impl FnMut(usize, &str) + Send {
        |_turn, _token: &str| {}
    }

    async fn send(
        manager: &InMemorySessionManager,
        id: &str,
        prompt: &str,
    ) -> anyhow::Result<Outcome> {
        let mut on_step = noop_step();
        let mut on_token = noop_token();
        manager.send(id, prompt, &mut on_step, &mut on_token).await
    }

    async fn resume(
        manager: &InMemorySessionManager,
        id: &str,
        decision: Decision,
    ) -> anyhow::Result<Outcome> {
        let mut on_step = noop_step();
        let mut on_token = noop_token();
        manager
            .resume(id, decision, &mut on_step, &mut on_token)
            .await
    }

    #[tokio::test]
    async fn create_produces_distinct_sessions_with_system_history() {
        let h = harness(scripted(Vec::new()), ApprovalPolicy::default());

        let first = h.manager.create(None).await.expect("create 失败");
        let second = h
            .manager
            .create(Some("u1".to_owned()))
            .await
            .expect("create 失败");

        assert_ne!(first.session_id, second.session_id);
        assert_eq!(first.history.len(), 1, "create 后历史里只有 system");
        assert!(first.state.is_empty());
        assert!(first.updated_at >= first.created_at);
        assert_eq!(second.user_id.as_deref(), Some("u1"));
        assert!(first.pending_call().is_none());
    }

    #[tokio::test]
    async fn get_and_delete_report_absence() {
        let h = harness(scripted(Vec::new()), ApprovalPolicy::default());
        let created = h.manager.create(None).await.expect("create 失败");

        assert!(h.manager.get("missing").await.expect("get 失败").is_none());
        assert!(
            h.manager
                .get(&created.session_id)
                .await
                .expect("get 失败")
                .is_some()
        );
        assert!(h.manager.pending("missing").await.is_err(), "id 错了应报错");

        assert!(
            h.manager
                .delete(&created.session_id)
                .await
                .expect("delete 失败")
        );
        assert!(
            !h.manager
                .delete(&created.session_id)
                .await
                .expect("delete 失败")
        );
        assert!(
            h.manager
                .get(&created.session_id)
                .await
                .expect("get 失败")
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_filters_by_user_and_lists_newest_first() {
        let h = harness(
            scripted(vec![final_answer("完成")]),
            ApprovalPolicy::default(),
        );
        let idle = h
            .manager
            .create(Some("u1".to_owned()))
            .await
            .expect("create 失败");
        let active = h
            .manager
            .create(Some("u1".to_owned()))
            .await
            .expect("create 失败");
        let other = h
            .manager
            .create(Some("u2".to_owned()))
            .await
            .expect("create 失败");

        send(&h.manager, &active.session_id, "提问")
            .await
            .expect("send 失败");

        let all = h.manager.list(None).await.expect("list 失败");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].session_id, active.session_id, "最近更新的排前面");
        assert_eq!(all[0].title, "提问");
        assert!(all[0].message_count > 1);

        let only_u1 = h.manager.list(Some("u1")).await.expect("list 失败");
        let ids: Vec<&str> = only_u1.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains(&idle.session_id.as_str()));
        assert!(ids.contains(&active.session_id.as_str()));
        assert!(!ids.contains(&other.session_id.as_str()));
    }

    #[tokio::test]
    async fn send_accumulates_multi_turn_history() {
        let llm = scripted(vec![final_answer("第一答"), final_answer("第二答")]);
        let h = harness(llm.clone(), ApprovalPolicy::default());
        let session = h.manager.create(None).await.expect("create 失败");

        send(&h.manager, &session.session_id, "第一问")
            .await
            .expect("send 失败");
        let outcome = send(&h.manager, &session.session_id, "第二问")
            .await
            .expect("send 失败");

        assert_eq!(outcome.answer, "第二答");
        let requests = seen(&llm);
        assert_eq!(requests.len(), 2, "多轮 = 两次请求，共用同一个 loop");
        let second_request = &requests[1];
        let text = serde_json::to_string(second_request).expect("序列化失败");
        assert!(text.contains("第一问"), "第二轮应看到第一轮的提问");
        assert!(text.contains("第一答"), "第二轮应看到第一轮的回答");

        let stored = h
            .manager
            .get(&session.session_id)
            .await
            .expect("get 失败")
            .expect("会话应存在");
        assert!(stored.pending_call().is_none());
        assert!(stored.state.is_empty(), "正常收尾后不留保留键");
    }

    #[tokio::test]
    async fn ask_without_confirmer_suspends_and_records_state() {
        let llm = scripted(vec![call("call_probe", "probe", "{}")]);
        let h = harness(llm, ask_for("probe"));
        let session = h.manager.create(None).await.expect("create 失败");

        let outcome = send(&h.manager, &session.session_id, "跑一下")
            .await
            .expect("挂起不是错误");

        assert_eq!(outcome.termination, Termination::Suspended);
        assert_eq!(h.executed.load(Ordering::Relaxed), 0, "未获批准不得执行");

        let stored = h
            .manager
            .get(&session.session_id)
            .await
            .expect("get 失败")
            .expect("会话应存在");
        assert!(stored.pending_call().is_some(), "快照末尾应是未配对调用");
        assert_eq!(stored.suspended_turn(), Some(1));
        assert!(stored.state.contains_key(state_keys::PAUSE));

        let pending = h
            .manager
            .pending(&session.session_id)
            .await
            .expect("pending 失败")
            .expect("应有待审内容");
        assert_eq!(pending.tool_call_id, "call_probe");
        assert_eq!(pending.request.tool, "probe");
        assert_eq!(pending.request.turn, 1);
        assert_eq!(pending.request.description, "探针工具");

        let summary = h.manager.list(None).await.expect("list 失败");
        assert!(summary[0].suspended);
        assert_eq!(summary[0].pending_tool.as_deref(), Some("probe"));
    }

    #[tokio::test]
    async fn resume_approve_executes_pending_call_once_and_finishes() {
        let llm = scripted(vec![
            call("call_probe", "probe", "{}"),
            final_answer("办好了"),
        ]);
        let h = harness(llm, ask_for("probe"));
        let session = h.manager.create(None).await.expect("create 失败");
        send(&h.manager, &session.session_id, "跑一下")
            .await
            .expect("send 失败");

        let outcome = resume(&h.manager, &session.session_id, Decision::Approve)
            .await
            .expect("resume 失败");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(outcome.answer, "办好了");
        assert_eq!(h.executed.load(Ordering::Relaxed), 1, "恰好执行一次");

        let stored = h
            .manager
            .get(&session.session_id)
            .await
            .expect("get 失败")
            .expect("会话应存在");
        assert!(stored.pending_call().is_none());
        assert!(
            !stored.state.contains_key(state_keys::PAUSE),
            "收尾后清理保留键"
        );
        assert!(!stored.state.contains_key(state_keys::TURN));
        assert!(stored.state.is_empty());
    }

    #[tokio::test]
    async fn resume_deny_becomes_observation_and_loop_continues() {
        let llm = scripted(vec![
            call("call_probe", "probe", "{}"),
            final_answer("换个办法"),
        ]);
        let h = harness(llm, ask_for("probe"));
        let session = h.manager.create(None).await.expect("create 失败");
        send(&h.manager, &session.session_id, "跑一下")
            .await
            .expect("send 失败");

        let outcome = resume(&h.manager, &session.session_id, Decision::Deny)
            .await
            .expect("resume 失败");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(outcome.answer, "换个办法");
        assert_eq!(h.executed.load(Ordering::Relaxed), 0, "被拒的调用不得执行");

        let stored = h
            .manager
            .get(&session.session_id)
            .await
            .expect("get 失败")
            .expect("会话应存在");
        let text = serde_json::to_string(&stored.history).expect("序列化失败");
        assert!(text.contains("拒绝"), "拒绝应压成一条 Observation");
    }

    #[tokio::test]
    async fn send_on_suspended_and_resume_on_running_both_fail() {
        let llm = scripted(vec![call("call_probe", "probe", "{}")]);
        let h = harness(llm, ask_for("probe"));
        let session = h.manager.create(None).await.expect("create 失败");

        assert!(
            resume(&h.manager, &session.session_id, Decision::Approve)
                .await
                .is_err(),
            "非挂起态 resume 应报错"
        );

        send(&h.manager, &session.session_id, "跑一下")
            .await
            .expect("send 失败");
        assert!(
            send(&h.manager, &session.session_id, "再来一次")
                .await
                .is_err(),
            "挂起态 send 应报错，要求先 resume"
        );
    }

    #[tokio::test]
    async fn same_session_sends_are_serialized_without_losing_messages() {
        let llm = scripted(vec![final_answer("一"), final_answer("二")]);
        let h = harness(llm, ApprovalPolicy::default());
        let session = h.manager.create(None).await.expect("create 失败");

        let (first, second) = tokio::join!(
            send(&h.manager, &session.session_id, "甲"),
            send(&h.manager, &session.session_id, "乙"),
        );
        first.expect("并发 send 之一失败");
        second.expect("并发 send 之二失败");

        let stored = h
            .manager
            .get(&session.session_id)
            .await
            .expect("get 失败")
            .expect("会话应存在");
        let text = serde_json::to_string(&stored.history).expect("序列化失败");
        assert!(
            text.contains("甲") && text.contains("乙"),
            "两条提问都不能丢"
        );
        assert_eq!(
            stored
                .history
                .iter()
                .filter(|m| matches!(m, ChatCompletionRequestMessage::User(_)))
                .count(),
            2,
            "串行执行后两条 user 消息都在"
        );
    }

    #[tokio::test]
    async fn different_sessions_do_not_block_each_other() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let llm = Arc::new(
            LLMClient::scripted(vec![final_answer("完成"), final_answer("完成")]).with_callbacks(
                vec![Arc::new(Gate {
                    marker: "【阻塞标记】".to_owned(),
                    entered: entered.clone(),
                    release: release.clone(),
                })],
            ),
        );
        let h = harness(llm, ApprovalPolicy::default());
        let blocked = h.manager.create(None).await.expect("create 失败");
        let free = h.manager.create(None).await.expect("create 失败");

        let manager = h.manager.clone();
        let blocked_id = blocked.session_id.clone();
        let blocked_task = tokio::spawn(async move {
            send(manager.as_ref(), &blocked_id, "【阻塞标记】先跑这个").await
        });

        entered.notified().await;

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            send(&h.manager, &free.session_id, "别的会话"),
        )
        .await
        .expect("另一个会话不应被阻塞的会话卡住")
        .expect("send 失败");
        assert_eq!(outcome.termination, Termination::FinalAnswer);

        release.notify_one();
        blocked_task
            .await
            .expect("任务 panic")
            .expect("被阻塞的会话应能正常结束");
    }

    #[tokio::test]
    async fn suspension_survives_other_activity_and_stays_resumable() {
        let llm = scripted(vec![
            call("call_probe", "probe", "{}"),
            // 第二个会话的收尾。
            final_answer("别的会话完成"),
            // 原会话 resume 后的收尾。
            final_answer("完成"),
        ]);
        let h = harness(llm, ask_for("probe"));
        let session = h.manager.create(None).await.expect("create 失败");
        send(&h.manager, &session.session_id, "跑一下")
            .await
            .expect("send 失败");

        // 一圈无关操作：列会话、读会话、另开一个会话并跑一轮。
        h.manager.list(None).await.expect("list 失败");
        h.manager.get(&session.session_id).await.expect("get 失败");
        let other = h.manager.create(None).await.expect("create 失败");
        send(&h.manager, &other.session_id, "别的事")
            .await
            .expect("send 失败");

        let pending_before = h
            .manager
            .pending(&session.session_id)
            .await
            .expect("pending 失败");
        assert!(pending_before.is_some());

        let outcome = resume(&h.manager, &session.session_id, Decision::Approve)
            .await
            .expect("挂起应无限期可恢复");
        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(outcome.answer, "完成");
    }
}
