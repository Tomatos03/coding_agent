//! 内置的应用策略：[`AgentEvaluator`] 持有一个 [`Agent`]，负责解析并执行斜杠命令、
//! 维护会话，以及对普通输入驱动一轮 ReAct。
//!
//! 命令协议（`/` 前缀、命令词表、提示文案）都是这一层的实现细节；换一个 [`Evaluator`]
//! 实现即可换掉整套交互。想自己控制会话也可直接用 `Agent::send` / `Agent::resume`。

use async_trait::async_trait;

use crate::react::approval::Decision;
use crate::react::models::{Outcome, Step, Termination};
use crate::runtime::Agent;

use super::{Emit, Evaluator};

/// 展示载荷：命令结果 / 提示 / 错误，以及编排层的步骤事件。
///
/// 这是 AgentEvaluator 这层的协议类型，[`StdoutWriter`](super::StdoutWriter) 等
/// [`Writer`](super::Writer) 实现直接消费它；框架不认识。
#[derive(Debug)]
pub enum Output {
    /// 已定稿的一行文本：命令结果、提示、错误。
    Message(String),
    /// 编排层的步骤事件（思考 / 动作 / 观察 / 答案），怎么渲染由 Writer 决定。
    Step(Step),
}

/// 内置 Evaluator：持有一个 [`Agent`]，并维护当前活跃会话
/// （首次追问时懒建，`/new` `/switch` `/delete` 改它）。
pub struct AgentEvaluator {
    agent: Agent,
    active: Option<String>,
}

impl AgentEvaluator {
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

    /// 没有活跃会话就新建一个。
    async fn ensure_session(&mut self, out: &mut Vec<Output>) -> anyhow::Result<String> {
        if let Some(id) = self.active.as_ref() {
            return Ok(id.clone());
        }
        let session = self.agent.new_session().await?;
        out.push(Output::Message(format!(
            "已新建会话 {}（{}）",
            short_id(&session.session_id),
            session.title()
        )));
        self.active = Some(session.session_id.clone());
        Ok(session.session_id)
    }

    async fn send_turn(
        &self,
        out: &mut Vec<Output>,
        session_id: &str,
        prompt: &str,
    ) -> anyhow::Result<Outcome> {
        let mut on_step = |step: &Step| out.push(Output::Step(step.clone()));
        // 流式 token 暂不投递：`step` 已交付完整答案，同时投递会打印两遍。
        let mut on_token = |_turn: usize, _token: &str| {};
        self.agent
            .send(session_id, prompt, &mut on_step, &mut on_token)
            .await
    }

    async fn resume_turn(
        &self,
        out: &mut Vec<Output>,
        session_id: &str,
        decision: Decision,
    ) -> anyhow::Result<Outcome> {
        let mut on_step = |step: &Step| out.push(Output::Step(step.clone()));
        let mut on_token = |_turn: usize, _token: &str| {};
        self.agent
            .resume(session_id, decision, &mut on_step, &mut on_token)
            .await
    }

    /// 斜杠命令。返回 `false` 表示请求退出循环（由 [`Evaluator::eval`] 折成
    /// [`Emit::Quit`]）。
    async fn handle_command(
        &mut self,
        out: &mut Vec<Output>,
        command: &str,
    ) -> anyhow::Result<bool> {
        let mut parts = command.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();

        match name {
            "help" => print_help(out),
            "quit" | "exit" => return Ok(false),
            "new" => {
                let session = self.agent.new_session().await?;
                out.push(Output::Message(format!(
                    "已新建会话 {}（{}）",
                    short_id(&session.session_id),
                    session.title()
                )));
                self.active = Some(session.session_id);
            }
            "sessions" => {
                let summaries = self.agent.list().await?;
                if summaries.is_empty() {
                    out.push(Output::Message("（还没有会话）".to_owned()));
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
                    out.push(Output::Message(format!(
                        "{mark} {}  {}  {} 条消息{suspended}",
                        short_id(&summary.session_id),
                        summary.title,
                        summary.message_count
                    )));
                }
            }
            "switch" => match rest.first() {
                Some(prefix) => {
                    if let Some(id) = self.resolve(out, prefix).await? {
                        let title = self
                            .agent
                            .get(&id)
                            .await?
                            .map(|session| session.title())
                            .unwrap_or_default();
                        out.push(Output::Message(format!(
                            "已切到 {}（{title}）",
                            short_id(&id)
                        )));
                        self.active = Some(id);
                    }
                }
                None => out.push(Output::Message("用法：/switch <id前缀>".to_owned())),
            },
            "delete" => match rest.first() {
                Some(prefix) => {
                    if let Some(id) = self.resolve(out, prefix).await? {
                        let deleted = self.agent.delete(&id).await?;
                        out.push(Output::Message(format!(
                            "已删除 {}：{deleted}",
                            short_id(&id)
                        )));
                        if self.active.as_deref() == Some(id.as_str()) {
                            self.active = None;
                        }
                    }
                }
                None => out.push(Output::Message("用法：/delete <id前缀>".to_owned())),
            },
            "status" => match self.active.as_ref() {
                Some(id) => match self.agent.get(id).await? {
                    Some(session) => {
                        let state = match session.pending_call() {
                            Some(call) => format!("挂起，待审工具 `{}`", call.tool),
                            None => "空闲".to_owned(),
                        };
                        out.push(Output::Message(format!(
                            "{}  {}  {} 条消息  {state}",
                            short_id(id),
                            session.title(),
                            session.history.len()
                        )));
                    }
                    None => out.push(Output::Message("当前会话已不存在。".to_owned())),
                },
                None => out.push(Output::Message(
                    "还没有会话，输入内容或 `/new` 开始一段。".to_owned(),
                )),
            },
            "resume" => {
                let (id, decision) = match rest.as_slice() {
                    ["y"] | ["Y"] => (self.active.clone(), Some(Decision::Approve)),
                    ["n"] | ["N"] => (self.active.clone(), Some(Decision::Deny)),
                    [prefix, "y"] | [prefix, "Y"] => {
                        (self.resolve(out, prefix).await?, Some(Decision::Approve))
                    }
                    [prefix, "n"] | [prefix, "N"] => {
                        (self.resolve(out, prefix).await?, Some(Decision::Deny))
                    }
                    _ => (None, None),
                };
                match (id, decision) {
                    (Some(id), Some(decision)) => {
                        let outcome = self.resume_turn(out, &id, decision).await?;
                        report(out, &outcome);
                    }
                    _ => out.push(Output::Message("用法：/resume [<id前缀>] <y|n>".to_owned())),
                }
            }
            other => out.push(Output::Message(format!(
                "未知命令 `/{other}`，输入 `/help` 查看。"
            ))),
        }

        Ok(true)
    }

    /// 唯一前缀匹配；返回完整 id（无匹配 / 歧义时已写入提示）。
    async fn resolve(&self, out: &mut Vec<Output>, prefix: &str) -> anyhow::Result<Option<String>> {
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
                out.push(Output::Message(format!("没有匹配 `{prefix}` 的会话。")));
                Ok(None)
            }
            _ => {
                out.push(Output::Message(format!(
                    "前缀 `{prefix}` 有歧义，匹配到 {} 个会话。",
                    matches.len()
                )));
                Ok(None)
            }
        }
    }
}

#[async_trait]
impl Evaluator<String> for AgentEvaluator {
    type Out = Output;

    /// 交互式会话循环的单步：解析斜杠命令并执行，或把普通输入当作对当前会话的追问
    /// 驱动一轮 ReAct。
    ///
    /// 单轮出错（网络、挂起态被追问……）折成一行 `[错误]` 输出并继续——交互场景下
    /// 把整个对话丢掉比报错更糟；返回 `Err` 只留给真正的致命情况。
    /// `/quit` 折成 [`Emit::Quit`] 放在产出末尾。
    async fn eval(&mut self, input: String) -> anyhow::Result<Vec<Emit<Output>>> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(command) = input.strip_prefix('/') {
            let mut out = Vec::new();
            return match self.handle_command(&mut out, command).await {
                Ok(true) => Ok(into_emits(out)),
                Ok(false) => {
                    let mut emits = into_emits(out);
                    emits.push(Emit::Quit);
                    Ok(emits)
                }
                Err(error) => {
                    out.push(Output::Message(format!("[错误] {error}")));
                    Ok(into_emits(out))
                }
            };
        }

        let mut out = Vec::new();
        match self.ensure_session(&mut out).await {
            Ok(id) => match self.send_turn(&mut out, &id, input).await {
                Ok(outcome) => report(&mut out, &outcome),
                Err(error) => out.push(Output::Message(format!("[错误] {error}"))),
            },
            Err(error) => out.push(Output::Message(format!("[错误] {error}"))),
        }
        Ok(into_emits(out))
    }
}

/// 把一批展示载荷包成「继续」的 [`Emit`] 序列；退出请求由调用处追加。
fn into_emits(out: Vec<Output>) -> Vec<Emit<Output>> {
    out.into_iter().map(Emit::Continue).collect()
}

/// id 前 8 位，够命令行里辨认。
fn short_id(session_id: &str) -> &str {
    &session_id[..session_id.len().min(8)]
}

/// 一轮结束时向用户交代结果：正常收尾报终止原因，挂起则给出下一步提示。
fn report(out: &mut Vec<Output>, outcome: &Outcome) {
    match outcome.termination {
        Termination::Suspended => match &outcome.pending {
            Some(pending) => {
                out.push(Output::Message(format!(
                    "[挂起] 工具 `{}` 待审批：{}",
                    pending.request.tool, pending.request.arguments
                )));
                out.push(Output::Message(
                    "       用 `/resume y` 批准、`/resume n` 拒绝；挂起会一直保留。".to_owned(),
                ));
            }
            None => out.push(Output::Message(
                "[挂起] 会话停在半途，用 `/resume <y|n>` 继续。".to_owned(),
            )),
        },
        other => out.push(Output::Message(format!(
            "--- 终止于 {other:?}，共 {} 轮 ---",
            outcome.turns
        ))),
    }
}

fn print_help(out: &mut Vec<Output>) {
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
        out.push(Output::Message(line.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, FunctionCall,
    };
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::sync::Arc;

    use super::*;
    use crate::llm::models::{LLMClient, Reply};
    use crate::react::models::DEFAULT_MAX_TURNS;
    use crate::settings::{ApprovalAction, ApprovalPolicy, ApprovalRule};
    use crate::tools::ToolHashMap;
    use crate::tools::local::final_answer::{FINAL_ANSWER_TOOL, FinalAnswer};
    use crate::tools::tool::Tool;

    /// 把产出里的文本行拼起来，便于断言；步骤事件单独计数。
    fn messages(emits: &[Emit<Output>]) -> String {
        emits
            .iter()
            .filter_map(|emitted| match emitted {
                Emit::Continue(Output::Message(line)) => Some(line.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn step_count(emits: &[Emit<Output>]) -> usize {
        emits
            .iter()
            .filter(|emitted| matches!(emitted, Emit::Continue(Output::Step(_))))
            .count()
    }

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

    // ---- AgentEvaluator::eval（把交互协议钉住）----

    #[tokio::test]
    async fn eval_creates_a_session_and_reports_the_answer() {
        let mut evaluator = AgentEvaluator::new(agent());

        let outputs = evaluator.eval("提问".to_owned()).await.expect("eval 失败");

        let text = messages(&outputs);
        assert!(text.contains("已新建会话"), "首次追问应自动建会话：{text}");
        assert!(text.contains("终止于 FinalAnswer"), "{text}");
        assert!(step_count(&outputs) > 0, "step 事件应在输出里");
    }

    #[tokio::test]
    async fn eval_dispatches_slash_commands_and_quits() {
        let mut evaluator = AgentEvaluator::new(agent());
        let mut all = Vec::new();

        for line in ["/help", "/new", "/sessions", "/status"] {
            let outputs = evaluator.eval(line.to_owned()).await.expect("eval 失败");
            assert!(
                !outputs.iter().any(|o| matches!(o, Emit::Quit)),
                "`{line}` 不应退出"
            );
            all.extend(outputs);
        }
        let empty = evaluator.eval(String::new()).await.expect("eval 失败");
        assert!(empty.is_empty(), "空输入不该产出输出");
        let quit = evaluator
            .eval("/quit".to_owned())
            .await
            .expect("eval 失败");
        assert!(
            matches!(quit.as_slice(), [Emit::Quit]),
            "/quit 应产出唯一的 Quit"
        );

        let text = messages(&all);
        assert!(
            text.contains("/resume [<id前缀>] <y|n>"),
            "help 应列出命令：{text}"
        );
        assert!(text.contains("已新建会话"), "{text}");
        assert!(
            text.contains("条消息"),
            "sessions/status 应报消息数：{text}"
        );
        assert!(text.contains("空闲"), "{text}");
        assert!(!text.contains("再见"), "收尾是示例的事，不在库里");
        assert!(!text.contains("终止于"), "只有命令，没有真的追问：{text}");
        assert_eq!(step_count(&all), 0, "全程没有真的追问");
    }

    #[tokio::test]
    async fn eval_reports_suspension_and_resume_command_continues() {
        let mut evaluator = AgentEvaluator::new(asking_agent(vec![
            tool_call("call_probe", "probe"),
            final_reply("办好了"),
        ]));

        let outputs = evaluator
            .eval("跑一下".to_owned())
            .await
            .expect("eval 失败");
        let text = messages(&outputs);
        assert!(text.contains("[挂起]"), "第一次追问应报挂起：{text}");
        assert!(text.contains("/resume y"), "挂起提示应给出下一步：{text}");

        let outputs = evaluator
            .eval("/resume y".to_owned())
            .await
            .expect("eval 失败");
        assert!(
            messages(&outputs).contains("终止于 FinalAnswer"),
            "恢复后应能收尾：{}",
            messages(&outputs)
        );
    }

    #[tokio::test]
    async fn eval_prints_error_and_keeps_going() {
        let mut evaluator =
            AgentEvaluator::new(asking_agent(vec![tool_call("call_probe", "probe")]));
        let mut all = Vec::new();

        all.extend(
            evaluator
                .eval("跑一下".to_owned())
                .await
                .expect("eval 失败"),
        );
        // 挂起态再追问 → manager 报错；折成一行输出，不中断。
        let outputs = evaluator
            .eval("再问一句".to_owned())
            .await
            .expect("单轮错误不该让 eval 失败");
        assert!(!outputs.iter().any(|o| matches!(o, Emit::Quit)));
        all.extend(outputs);
        all.extend(
            evaluator
                .eval("/status".to_owned())
                .await
                .expect("eval 失败"),
        );

        let text = messages(&all);
        assert!(text.contains("[错误]"), "应打印错误：{text}");
        assert!(
            text.contains("挂起，待审工具 `probe`"),
            "交互应继续：{text}"
        );
    }
}
