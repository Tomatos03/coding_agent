//! 顶层 `Agent` 组件：组装一个 `SessionManager`，并把用户 API 委派给它。
//!
//! `Agent` 自己不跑循环——多轮、挂起、恢复都在
//! [`SessionManager`](crate::session::manager::SessionManager) 里，
//! 因为「每个 session 一个常驻 `ReactLoop`」这件事由它持有。

use std::sync::Arc;

use async_trait::async_trait;

use crate::llm::models::LLMClient;
use crate::react::approval::{Confirmer, Decision};
use crate::react::models::{Outcome, PendingApproval, Step, Termination};
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

    /// 交互式会话循环：读入 →（斜杠命令 | 追问）→ 驱动一轮 → 展示，直到输入结束或 `/quit`。
    ///
    /// 当前会话由循环自己维护（首次追问时自动新建、`/new` `/switch` `/delete` 改它）；
    /// 想自己控制会话请直接用 [`Agent::send`] / [`Agent::resume`]。
    ///
    /// 单轮出错（网络、挂起态被追问……）只打印一行 `[错误]` 并继续循环——交互场景下
    /// 把整个对话丢掉比报错更糟。
    pub async fn run(&self, console: &mut dyn Console) -> anyhow::Result<()> {
        let mut active: Option<String> = None;

        while let Some(line) = console.read_line().await {
            let input = line.trim();
            if input.is_empty() {
                continue;
            }

            if let Some(command) = input.strip_prefix('/') {
                match self.handle_command(console, &mut active, command).await {
                    Ok(true) => {}
                    // `/quit`
                    Ok(false) => return Ok(()),
                    Err(error) => console.print(&format!("[错误] {error}")),
                }
                continue;
            }

            let outcome = match self.ensure_session(console, &mut active).await {
                Ok(id) => self.send_turn(console, &id, input).await,
                Err(error) => Err(error),
            };
            match outcome {
                Ok(outcome) => report(console, &outcome),
                Err(error) => console.print(&format!("[错误] {error}")),
            }
        }

        Ok(())
    }

    /// 没有活跃会话就新建一个。
    async fn ensure_session(
        &self,
        console: &mut dyn Console,
        active: &mut Option<String>,
    ) -> anyhow::Result<String> {
        if let Some(id) = active.as_ref() {
            return Ok(id.clone());
        }
        let session = self.new_session().await?;
        console.print(&format!(
            "已新建会话 {}（{}）",
            short_id(&session.session_id),
            session.title()
        ));
        *active = Some(session.session_id.clone());
        Ok(session.session_id)
    }

    async fn send_turn(
        &self,
        console: &mut dyn Console,
        session_id: &str,
        prompt: &str,
    ) -> anyhow::Result<Outcome> {
        let mut on_step = |step: &Step| console.step(step);
        // 流式 token 暂不投递：`step` 已交付完整答案，同时投递会打印两遍。
        let mut on_token = |_turn: usize, _token: &str| {};
        self.send(session_id, prompt, &mut on_step, &mut on_token)
            .await
    }

    async fn resume_turn(
        &self,
        console: &mut dyn Console,
        session_id: &str,
        decision: Decision,
    ) -> anyhow::Result<Outcome> {
        let mut on_step = |step: &Step| console.step(step);
        let mut on_token = |_turn: usize, _token: &str| {};
        self.resume(session_id, decision, &mut on_step, &mut on_token)
            .await
    }

    /// 斜杠命令。返回 `false` 表示请求退出循环。
    async fn handle_command(
        &self,
        console: &mut dyn Console,
        active: &mut Option<String>,
        command: &str,
    ) -> anyhow::Result<bool> {
        let mut parts = command.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();

        match name {
            "help" => print_help(console),
            "quit" | "exit" => return Ok(false),
            "new" => {
                let session = self.new_session().await?;
                console.print(&format!(
                    "已新建会话 {}（{}）",
                    short_id(&session.session_id),
                    session.title()
                ));
                *active = Some(session.session_id);
            }
            "sessions" => {
                let summaries = self.list().await?;
                if summaries.is_empty() {
                    console.print("（还没有会话）");
                }
                for summary in summaries {
                    let mark = if Some(&summary.session_id) == active.as_ref() {
                        "*"
                    } else {
                        " "
                    };
                    let suspended = if summary.suspended {
                        format!(
                            "  [挂起：{}]",
                            summary.pending_tool.as_deref().unwrap_or("?")
                        )
                    } else {
                        String::new()
                    };
                    console.print(&format!(
                        "{mark} {}  {}  {} 条消息{suspended}",
                        short_id(&summary.session_id),
                        summary.title,
                        summary.message_count
                    ));
                }
            }
            "switch" => match rest.first() {
                Some(prefix) => {
                    if let Some(id) = self.resolve(console, prefix).await? {
                        let title = self
                            .get(&id)
                            .await?
                            .map(|session| session.title())
                            .unwrap_or_default();
                        console.print(&format!("已切到 {}（{title}）", short_id(&id)));
                        *active = Some(id);
                    }
                }
                None => console.print("用法：/switch <id前缀>"),
            },
            "delete" => match rest.first() {
                Some(prefix) => {
                    if let Some(id) = self.resolve(console, prefix).await? {
                        let deleted = self.delete(&id).await?;
                        console.print(&format!("已删除 {}：{deleted}", short_id(&id)));
                        if active.as_deref() == Some(id.as_str()) {
                            *active = None;
                        }
                    }
                }
                None => console.print("用法：/delete <id前缀>"),
            },
            "status" => match active.as_ref() {
                Some(id) => match self.get(id).await? {
                    Some(session) => {
                        let state = match session.pending_call() {
                            Some(call) => format!("挂起，待审工具 `{}`", call.tool),
                            None => "空闲".to_owned(),
                        };
                        console.print(&format!(
                            "{}  {}  {} 条消息  {state}",
                            short_id(id),
                            session.title(),
                            session.history.len()
                        ));
                    }
                    None => console.print("当前会话已不存在。"),
                },
                None => console.print("还没有会话，输入内容或 `/new` 开始一段。"),
            },
            "resume" => {
                let (id, decision) = match rest.as_slice() {
                    ["y"] | ["Y"] => (active.clone(), Some(Decision::Approve)),
                    ["n"] | ["N"] => (active.clone(), Some(Decision::Deny)),
                    [prefix, "y"] | [prefix, "Y"] => (
                        self.resolve(console, prefix).await?,
                        Some(Decision::Approve),
                    ),
                    [prefix, "n"] | [prefix, "N"] => {
                        (self.resolve(console, prefix).await?, Some(Decision::Deny))
                    }
                    _ => (None, None),
                };
                match (id, decision) {
                    (Some(id), Some(decision)) => {
                        let outcome = self.resume_turn(console, &id, decision).await?;
                        report(console, &outcome);
                    }
                    _ => console.print("用法：/resume [<id前缀>] <y|n>"),
                }
            }
            other => console.print(&format!("未知命令 `/{other}`，输入 `/help` 查看。")),
        }

        Ok(true)
    }

    /// 唯一前缀匹配；返回完整 id（无匹配 / 歧义时已打印提示）。
    async fn resolve(
        &self,
        console: &mut dyn Console,
        prefix: &str,
    ) -> anyhow::Result<Option<String>> {
        let matches: Vec<String> = self
            .list()
            .await?
            .into_iter()
            .map(|summary| summary.session_id)
            .filter(|id| id.starts_with(prefix))
            .collect();
        match matches.as_slice() {
            [only] => Ok(Some(only.clone())),
            [] => {
                console.print(&format!("没有匹配 `{prefix}` 的会话。"));
                Ok(None)
            }
            _ => {
                console.print(&format!(
                    "前缀 `{prefix}` 有歧义，匹配到 {} 个会话。",
                    matches.len()
                ));
                Ok(None)
            }
        }
    }
}

/// 交互式会话的 I/O 接缝：[`Agent::run`] 负责循环，读入与展示交给实现。
///
/// 拆成 trait 而不是在库里直接读写 stdin/stdout，`Agent` 才能继续做一个无隐式 IO 的
/// 库组件；示例接 stdin，测试接脚本化输入。
#[async_trait]
pub trait Console: Send {
    /// 读一条用户输入（实现通常在这里显示提示符）；`None` 表示输入结束。
    async fn read_line(&mut self) -> Option<String>;

    /// 输出一行：命令结果、提示与错误都走这里。
    fn print(&mut self, line: &str);

    /// 循环里的步骤事件（`Thought` / `Answer` / `Action` / `Observation`）。
    /// 不需要展示的实现可以不覆写。
    fn step(&mut self, _step: &Step) {}
}

/// id 前 8 位，够命令行里辨认。
fn short_id(session_id: &str) -> &str {
    &session_id[..session_id.len().min(8)]
}

/// 一轮结束时向用户交代结果：正常收尾报终止原因，挂起则给出下一步提示。
fn report(console: &mut dyn Console, outcome: &Outcome) {
    match outcome.termination {
        Termination::Suspended => match &outcome.pending {
            Some(pending) => {
                console.print(&format!(
                    "[挂起] 工具 `{}` 待审批：{}",
                    pending.request.tool, pending.request.arguments
                ));
                console.print("       用 `/resume y` 批准、`/resume n` 拒绝；挂起会一直保留。");
            }
            None => console.print("[挂起] 会话停在半途，用 `/resume <y|n>` 继续。"),
        },
        other => console.print(&format!(
            "--- 终止于 {other:?}，共 {} 轮 ---",
            outcome.turns
        )),
    }
}

fn print_help(console: &mut dyn Console) {
    for line in [
        "命令：",
        "  /help                        显示本帮助",
        "  /new                         新建会话并切过去",
        "  /sessions                    列出会话（* 为当前会话）",
        "  /switch <id前缀>             切换会话",
        "  /delete <id前缀>             删除会话",
        "  /status                      当前会话状态",
        "  /resume [<id前缀>] <y|n>     批准/拒绝挂起的调用并继续",
        "  /quit                        退出",
        "其它输入一律当作对当前会话的追问。",
    ] {
        console.print(line);
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
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::collections::VecDeque;

    use super::*;
    use crate::llm::models::Reply;
    use crate::react::models::DEFAULT_MAX_TURNS;
    use crate::settings::{ApprovalAction, ApprovalPolicy, ApprovalRule};
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

    // ---- Agent::run（把循环本身钉住）----

    /// 脚本化输入 + 内存输出：让 `Agent::run` 的循环可离线测。
    struct ScriptedConsole {
        inputs: VecDeque<String>,
        out: Vec<String>,
        steps: usize,
    }

    impl ScriptedConsole {
        fn new(inputs: &[&str]) -> Self {
            Self {
                inputs: inputs.iter().map(|line| (*line).to_owned()).collect(),
                out: Vec::new(),
                steps: 0,
            }
        }

        fn output(&self) -> String {
            self.out.join("\n")
        }
    }

    #[async_trait]
    impl Console for ScriptedConsole {
        async fn read_line(&mut self) -> Option<String> {
            self.inputs.pop_front()
        }

        fn print(&mut self, line: &str) {
            self.out.push(line.to_owned());
        }

        fn step(&mut self, _step: &Step) {
            self.steps += 1;
        }
    }

    /// 只用于让策略有东西可问；挂起发生在执行之前，它不会被真的调用。
    struct ProbeTool;

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            "probe"
        }

        fn description(&self) -> &str {
            "探针工具"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, args: &str) -> anyhow::Result<String> {
            Ok(format!("probe:{args}"))
        }
    }

    fn tool_call(id: &str, name: &str) -> Reply {
        Reply {
            content: String::new(),
            tool_calls: vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: id.to_owned(),
                    function: FunctionCall {
                        name: name.to_owned(),
                        arguments: "{}".to_owned(),
                    },
                },
            )],
        }
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

    /// 注入 `ask` 策略但不给确认方：第一次追问会挂起。
    fn asking_agent(replies: Vec<Reply>) -> Agent {
        let mut tools = ToolHashMap::new();
        tools.insert(
            FINAL_ANSWER_TOOL.to_owned(),
            Arc::new(FinalAnswer) as Arc<dyn Tool>,
        );
        tools.insert("probe".to_owned(), Arc::new(ProbeTool) as Arc<dyn Tool>);
        Agent::builder(
            scripted(replies),
            tools,
            "你是测试助手。",
            DEFAULT_MAX_TURNS,
        )
        .approval_policy(ApprovalPolicy {
            rules: vec![ApprovalRule {
                pattern: "probe".to_owned(),
                action: ApprovalAction::Ask,
            }],
            ..Default::default()
        })
        .in_memory()
    }

    #[tokio::test]
    async fn run_creates_a_session_and_reports_the_answer() {
        let agent = agent();
        let mut console = ScriptedConsole::new(&["提问"]);

        agent.run(&mut console).await.expect("run 失败");

        let out = console.output();
        assert!(out.contains("已新建会话"), "首次追问应自动建会话：{out}");
        assert!(out.contains("终止于 FinalAnswer"), "{out}");
        assert!(console.steps > 0, "step 事件应投递到 Console");
    }

    #[tokio::test]
    async fn run_dispatches_slash_commands_and_stops_on_quit() {
        let agent = agent();
        let mut console = ScriptedConsole::new(&[
            "/help",
            "/new",
            "/sessions",
            "/status",
            "/quit",
            "退出后不该再处理",
        ]);

        agent.run(&mut console).await.expect("run 失败");

        let out = console.output();
        assert!(
            out.contains("/resume [<id前缀>] <y|n>"),
            "help 应列出命令：{out}"
        );
        assert!(out.contains("已新建会话"), "{out}");
        assert!(out.contains("条消息"), "sessions/status 应报消息数：{out}");
        assert!(out.contains("空闲"), "{out}");
        assert!(!out.contains("再见"), "收尾是示例的事，不在库里");
        assert!(!out.contains("终止于"), "quit 之后不再处理输入：{out}");
        assert_eq!(console.steps, 0, "全程没有真的追问");
    }

    #[tokio::test]
    async fn run_reports_suspension_and_resume_command_continues() {
        let agent = asking_agent(vec![
            tool_call("call_probe", "probe"),
            final_reply("办好了"),
        ]);
        let mut console = ScriptedConsole::new(&["跑一下", "/resume y"]);

        agent.run(&mut console).await.expect("run 失败");

        let out = console.output();
        assert!(out.contains("[挂起]"), "第一次追问应报挂起：{out}");
        assert!(out.contains("/resume y"), "挂起提示应给出下一步：{out}");
        assert!(out.contains("终止于 FinalAnswer"), "恢复后应能收尾：{out}");
    }

    #[tokio::test]
    async fn run_prints_error_and_keeps_the_loop_alive() {
        let agent = asking_agent(vec![tool_call("call_probe", "probe")]);
        // 挂起态再追问 → manager 报错；循环只打印一行并继续处理 /status。
        let mut console = ScriptedConsole::new(&["跑一下", "再问一句", "/status"]);

        agent.run(&mut console).await.expect("单轮错误不该终止循环");

        let out = console.output();
        assert!(out.contains("[错误]"), "应打印错误：{out}");
        assert!(out.contains("挂起，待审工具 `probe`"), "循环应继续：{out}");
    }
}
