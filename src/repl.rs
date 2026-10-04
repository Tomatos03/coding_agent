//! 交互式 REPL 组件：持有一个 [`Agent`]，跑「读入 →（斜杠命令 | 追问）→ 驱动一轮 →
//! 展示」的循环。
//!
//! [`Agent`] 只组装配置、对外委派会话操作，不碰终端；[`Repl`] 通过 [`Console`] 接缝
//! 读入与展示，I/O 因此仍挡在库外（示例接 stdin，测试接脚本化输入）。

use async_trait::async_trait;

use crate::react::approval::Decision;
use crate::react::models::{Outcome, Step, Termination};
use crate::runtime::Agent;

/// 交互式 REPL：持有一个 [`Agent`]，并维护当前活跃会话。
pub struct Repl {
    agent: Agent,
    /// 当前活跃会话；首次追问时懒建。
    active: Option<String>,
}

impl Repl {
    /// 包住一个组装好的 [`Agent`]。
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            active: None,
        }
    }

    /// 拿到里层的 [`Agent`]（想直接用其 API 时用）。
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// 交互式会话循环：读入 →（斜杠命令 | 追问）→ 驱动一轮 → 展示，直到输入结束或 `/quit`。
    ///
    /// 当前会话由组件自己维护（首次追问时自动新建、`/new` `/switch` `/delete` 改它）；
    /// 想自己控制会话请直接用 [`Agent::send`] / [`Agent::resume`]。
    ///
    /// 单轮出错（网络、挂起态被追问……）只打印一行 `[错误]` 并继续循环——交互场景下
    /// 把整个对话丢掉比报错更糟。
    pub async fn run(&mut self, console: &mut dyn Console) -> anyhow::Result<()> {
        while let Some(line) = console.read_line().await {
            let input = line.trim();
            if input.is_empty() {
                continue;
            }

            if let Some(command) = input.strip_prefix('/') {
                match self.handle_command(console, command).await {
                    Ok(true) => {}
                    // `/quit`
                    Ok(false) => return Ok(()),
                    Err(error) => console.print(&format!("[错误] {error}")),
                }
                continue;
            }

            let outcome = match self.ensure_session(console).await {
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
    async fn ensure_session(&mut self, console: &mut dyn Console) -> anyhow::Result<String> {
        if let Some(id) = self.active.as_ref() {
            return Ok(id.clone());
        }
        let session = self.agent.new_session().await?;
        console.print(&format!(
            "已新建会话 {}（{}）",
            short_id(&session.session_id),
            session.title()
        ));
        self.active = Some(session.session_id.clone());
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
        self.agent
            .send(session_id, prompt, &mut on_step, &mut on_token)
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
        self.agent
            .resume(session_id, decision, &mut on_step, &mut on_token)
            .await
    }

    /// 斜杠命令。返回 `false` 表示请求退出循环。
    async fn handle_command(
        &mut self,
        console: &mut dyn Console,
        command: &str,
    ) -> anyhow::Result<bool> {
        let mut parts = command.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();

        match name {
            "help" => print_help(console),
            "quit" | "exit" => return Ok(false),
            "new" => {
                let session = self.agent.new_session().await?;
                console.print(&format!(
                    "已新建会话 {}（{}）",
                    short_id(&session.session_id),
                    session.title()
                ));
                self.active = Some(session.session_id);
            }
            "sessions" => {
                let summaries = self.agent.list().await?;
                if summaries.is_empty() {
                    console.print("（还没有会话）");
                }
                for summary in summaries {
                    let mark = if Some(&summary.session_id) == self.active.as_ref() {
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
                            .agent
                            .get(&id)
                            .await?
                            .map(|session| session.title())
                            .unwrap_or_default();
                        console.print(&format!("已切到 {}（{title}）", short_id(&id)));
                        self.active = Some(id);
                    }
                }
                None => console.print("用法：/switch <id前缀>"),
            },
            "delete" => match rest.first() {
                Some(prefix) => {
                    if let Some(id) = self.resolve(console, prefix).await? {
                        let deleted = self.agent.delete(&id).await?;
                        console.print(&format!("已删除 {}：{deleted}", short_id(&id)));
                        if self.active.as_deref() == Some(id.as_str()) {
                            self.active = None;
                        }
                    }
                }
                None => console.print("用法：/delete <id前缀>"),
            },
            "status" => match self.active.as_ref() {
                Some(id) => match self.agent.get(id).await? {
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
                    ["y"] | ["Y"] => (self.active.clone(), Some(Decision::Approve)),
                    ["n"] | ["N"] => (self.active.clone(), Some(Decision::Deny)),
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
            .agent
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

/// 交互式会话的 I/O 接缝：[`Repl::run`] 负责循环，读入与展示交给实现。
///
/// 拆成 trait 而不是在库里直接读写 stdin/stdout，交互层才能继续做一个无隐式 IO 的
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

#[cfg(test)]
mod tests {
    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, FunctionCall,
    };
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::sync::Arc;

    use super::*;
    use crate::llm::models::{LLMClient, Reply};
    use crate::react::models::DEFAULT_MAX_TURNS;
    use crate::settings::{ApprovalAction, ApprovalPolicy, ApprovalRule};
    use crate::tools::ToolHashMap;
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

    // ---- Repl::run（把循环本身钉住）----

    /// 脚本化输入 + 内存输出：让 `Repl::run` 的循环可离线测。
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

        let mut repl = Repl::new(agent);
        repl.run(&mut console).await.expect("run 失败");

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

        let mut repl = Repl::new(agent);
        repl.run(&mut console).await.expect("run 失败");

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

        let mut repl = Repl::new(agent);
        repl.run(&mut console).await.expect("run 失败");

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

        let mut repl = Repl::new(agent);
        repl.run(&mut console).await.expect("单轮错误不该终止循环");

        let out = console.output();
        assert!(out.contains("[错误]"), "应打印错误：{out}");
        assert!(out.contains("挂起，待审工具 `probe`"), "循环应继续：{out}");
    }
}
