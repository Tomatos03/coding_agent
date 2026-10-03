use std::sync::Arc;

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, ChatCompletionRequestMessage,
    FunctionCall,
};
use tracing::info;

use crate::agent::llm::models::{Completer, Reply, ToolPolicy};
use crate::agent::react::approval::{ApprovalRequest, Confirmer, Decision};
use crate::agent::react::context::{Event, EventName, ExecuteContext, Role, Status};
use crate::agent::react::history::History;
use crate::agent::react::models::{Outcome, Step, Termination};
use crate::settings::{ApprovalAction, ApprovalPolicy};
use crate::tools::ToolHashMap;
use crate::tools::local::final_answer::{self, FINAL_ANSWER_TOOL};

pub struct ReactLoop {
    completer: Arc<dyn Completer>,
    tools: ToolHashMap,
    history: History,
    max_turns: usize,
    approval_policy: ApprovalPolicy,
    confirmer: Option<Arc<dyn Confirmer>>,
}

impl ReactLoop {
    /// `tools` 必须包含 `final_answer`：收尾轮会以 `tool_choice` 具名强制调用它，
    /// 缺失时服务端会因函数未声明而拒绝。用 [`crate::tools::build_tools`] 构建的
    /// 工具表天然满足；手工拼表的调用方需自行注册。
    pub fn new(
        completer: Arc<dyn Completer>,
        tools: ToolHashMap,
        system_prompt: &str,
        max_turns: usize,
    ) -> anyhow::Result<Self> {
        let mut history = History::new();
        history.system(system_prompt)?;

        Ok(Self {
            completer,
            tools,
            history,
            max_turns,
            approval_policy: ApprovalPolicy::default(),
            confirmer: None,
        })
    }

    /// 注入审批策略（通常来自 [`crate::settings::load_settings`]）。
    ///
    /// 不调用则全放行，行为与没有闸门时一致。加载是调用方的职责——
    /// `ReactLoop` 自己不读文件，保持无 IO、可离线测试。
    pub fn with_approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        self.approval_policy = policy;
        self
    }

    /// 注入确认方。策略判 `ask` 而没有 confirmer 时，调用会被拒绝（fail-closed）。
    pub fn with_confirmer(mut self, confirmer: Arc<dyn Confirmer>) -> Self {
        self.confirmer = Some(confirmer);
        self
    }

    pub fn history(&self) -> &[ChatCompletionRequestMessage] {
        self.history.as_slice()
    }

    pub async fn run(
        &mut self,
        prompt: &str,
        mut on_step: impl FnMut(&Step),
        mut on_token: impl FnMut(usize, &str) + Send,
    ) -> anyhow::Result<Outcome> {
        let mut context = ExecuteContext::new();

        let outcome = self
            .run_loop(prompt, &mut context, &mut on_step, &mut on_token)
            .await;

        info!("actual execute turns: {}", context.turn());
        outcome
    }

    async fn run_loop(
        &mut self,
        prompt: &str,
        context: &mut ExecuteContext,
        on_step: &mut impl FnMut(&Step),
        on_token: &mut (impl FnMut(usize, &str) + Send),
    ) -> anyhow::Result<Outcome> {
        self.history.user(prompt)?;

        for turn in 1..=self.max_turns {
            context.set_turn(turn);

            let progressed = self
                .run_turn(turn, &mut *context, &mut *on_step, &mut *on_token)
                .await;

            observe(context);

            if let Some(outcome) = progressed? {
                context.set_status(Status::Completed);
                return Ok(outcome);
            }
        }

        context.set_status(Status::Completed);
        let answer = self.finalize(context, on_step, on_token).await?;
        Ok(Outcome {
            answer,
            turns: self.max_turns,
            termination: Termination::MaxTurns,
        })
    }

    async fn run_turn(
        &mut self,
        turn: usize,
        context: &mut ExecuteContext,
        on_step: &mut impl FnMut(&Step),
        on_token: &mut (impl FnMut(usize, &str) + Send),
    ) -> anyhow::Result<Option<Outcome>> {
        let Reply {
            content,
            tool_calls,
        } = self
            .thinking(turn, &mut *context, &mut *on_step, &mut *on_token)
            .await?;

        // 只把能执行、能配对的 function 调用落库：`Custom` 等变体本仓库无法处理，
        // 写进去只会留下无人应答的 tool_call，让历史结构非法。
        let calls = function_calls(&tool_calls);

        self.history.assistant(&content, calls.clone())?;

        if calls.is_empty() {
            if content.is_empty() {
                tracing::warn!("模型在 required 下既没有 tool_calls 也没有内容");
                return Ok(Some(Outcome {
                    answer: content,
                    turns: turn,
                    termination: Termination::EmptyReply,
                }));
            }

            // `required` 下服务端必须给出 tool_call；走到这里通常是端点无视了
            // tool_choice（也可能是本轮只有非 function 类型的调用，已在上文过滤掉）。
            // 已经拿到一段完整文本，直接当答案收尾，别把它浪费掉。
            tracing::warn!("端点无视 tool_choice=required，按纯文本答案处理");
            return Ok(answer(
                content,
                turn,
                context,
                on_step,
                Termination::ModelFinished,
            ));
        }

        // `final_answer` 是「交付」而不是工具：参数即答案，不执行、不发 Observation。
        // 判据与顺序无关——本轮只要出现参数合法的调用就交付；同轮其它调用一律不执行
        // （答案已定，副作用不该再发生），但都要补上配对 tool 消息（见 `pair_delivery`）。
        if let Some((delivered, text)) = find_final_answer(&calls) {
            self.pair_delivery(context, &calls, delivered, &text)?;
            return Ok(answer(
                text,
                turn,
                context,
                on_step,
                Termination::FinalAnswer,
            ));
        }

        for call in &calls {
            // `function_calls` 已保证只剩 Function 变体。
            let ChatCompletionMessageToolCalls::Function(func_call) = call else {
                continue;
            };

            let observation = self
                .action(&func_call.function, turn, context, on_step)
                .await;
            self.on_observation(func_call, observation, turn, context, on_step)?;
        }

        Ok(None)
    }

    fn on_observation(
        &mut self,
        func_call: &ChatCompletionMessageToolCall,
        observation: String,
        turn: usize,
        context: &mut ExecuteContext,
        on_step: &mut impl FnMut(&Step),
    ) -> anyhow::Result<()> {
        on_step(&Step::Observation {
            turn,
            name: func_call.function.name.clone(),
            output: observation.clone(),
        });
        context.push_event(Event::new(
            EventName::ToolResult,
            observation.clone(),
            Role::Tool,
        ));
        self.history.tool(&func_call.id, &observation)?;
        Ok(())
    }

    /// 交付路径的历史落库：为本轮每个 `tool_call` 补一条配对 tool 消息。
    ///
    /// - `delivered` 那条写答案本身，历史因此始终可重放；
    /// - 其余调用从未执行，写 [`SKIPPED_CALL_NOTE`] 占位说明原因；
    /// - 这里一个工具都不执行，所以不发 `Step`——交付只有一个 `Step::Answer`。
    fn pair_delivery(
        &mut self,
        context: &mut ExecuteContext,
        calls: &[ChatCompletionMessageToolCalls],
        delivered: &ChatCompletionMessageToolCall,
        answer: &str,
    ) -> anyhow::Result<()> {
        let mut skipped = Vec::new();
        for call in calls {
            let ChatCompletionMessageToolCalls::Function(func) = call else {
                continue;
            };
            let content = if func.id == delivered.id {
                self.history.tool(&func.id, answer)?;
                answer
            } else {
                self.history.tool(&func.id, SKIPPED_CALL_NOTE)?;
                skipped.push(func.function.name.clone());
                SKIPPED_CALL_NOTE
            };
            context.push_event(Event::new(EventName::ToolResult, content, Role::Tool));
        }

        if !skipped.is_empty() {
            tracing::warn!(
                "final_answer 已交付，同轮 {} 个调用未执行：{}",
                skipped.len(),
                skipped.join(", ")
            );
        }

        Ok(())
    }

    async fn action(
        &mut self,
        func: &FunctionCall,
        turn: usize,
        context: &mut ExecuteContext,
        on_step: &mut impl FnMut(&Step),
    ) -> String {
        on_step(&Step::Action {
            turn,
            name: func.name.clone(),
            arguments: func.arguments.clone(),
        });
        context.push_event(Event::new(
            EventName::ToolCall,
            func.arguments.clone(),
            Role::Assistant,
        ));

        // 审批闸门：发完 Step::Action（模型确实做了这个动作）之后、execute 之前。
        // 拒绝同样是一条 Observation，与「工具失败 / 未知工具」走完全相同的通道，
        // 因此不变量①②照常成立，消费方也不用学新事件类型。
        match self.approval_policy.action_for(&func.name) {
            ApprovalAction::Allow => {}
            ApprovalAction::Ask => {
                let request = ApprovalRequest {
                    turn,
                    tool: func.name.clone(),
                    description: self
                        .tools
                        .get(&func.name)
                        .map(|tool| tool.description().to_owned())
                        .unwrap_or_default(),
                    arguments: func.arguments.clone(),
                };
                let decision = match &self.confirmer {
                    Some(confirmer) => confirmer.confirm(&request).await,
                    // 策略要问但无人可问 → fail-closed。
                    None => Decision::Deny,
                };
                if let Decision::Deny = decision {
                    tracing::warn!(tool = %func.name, "工具调用被用户拒绝");
                    return format!(
                        "用户拒绝执行工具 `{}`。请不要原样重试，先说明用途或改用其它方案。",
                        func.name
                    );
                }
            }
        }

        self.execute(&func.name, &func.arguments).await
    }

    async fn thinking(
        &mut self,
        turn: usize,
        context: &mut ExecuteContext,
        on_step: &mut impl FnMut(&Step),
        on_token: &mut (impl FnMut(usize, &str) + Send),
    ) -> anyhow::Result<Reply> {
        let reply = self
            .completer
            .stream(
                self.history.as_slice(),
                Some(&self.tools),
                ToolPolicy::Required,
                &mut |token| on_token(turn, token),
            )
            .await?;

        // required 下服务端保证有 tool_calls：content 只能是 thought。
        // 「有 content、无 tool_calls」是端点无视强制的降级信号，由 run_turn 处理。
        if !reply.content.is_empty() && !reply.tool_calls.is_empty() {
            on_step(&Step::Thought {
                turn,
                content: reply.content.clone(),
            });
            context.push_event(Event::new(
                EventName::Thought,
                reply.content.clone(),
                Role::Assistant,
            ));
        }

        Ok(reply)
    }

    async fn execute(&self, name: &str, arguments: &str) -> String {
        match self.tools.get(name) {
            Some(tool) => tool
                .execute(arguments)
                .await
                .unwrap_or_else(|e| format!("工具执行失败：{e}")),
            None => format!("未知工具：{name}"),
        }
    }

    async fn finalize(
        &mut self,
        context: &mut ExecuteContext,
        on_step: &mut impl FnMut(&Step),
        on_token: &mut (impl FnMut(usize, &str) + Send),
    ) -> anyhow::Result<String> {
        // 用 system 指令而不是 user 消息收尾：这是对「本轮该如何作答」的运行期约束，
        // 与初始 system prompt 同类，也避免被后续 user/assistant 轮次稀释。
        self.history.system(FINALIZE_INSTRUCTION)?;

        let turn = self.max_turns + 1;
        context.set_turn(turn);

        // 动态裁剪工具面到只剩 final_answer：即使端点忽略了具名强制，模型也没有
        // 旁路工具可调，只能交付答案（同时省掉一整车无关的 function 定义）。
        let tools = finalize_tools(&self.tools);

        let reply = self
            .completer
            .stream(
                self.history.as_slice(),
                tools.as_ref(),
                ToolPolicy::Force(FINAL_ANSWER_TOOL.to_owned()),
                &mut |token| on_token(turn, token),
            )
            .await?;

        // 与 run_turn 一致：只把能配对的 function 调用落库；并行开启后收尾轮
        // 同样可能一次带回多个 call。
        let calls = function_calls(&reply.tool_calls);

        // 注意这里要的是「原始调用」而非 `find_final_answer`：收尾轮没有下一轮
        // 重试，参数非法也必须照样补配对消息，所以不能复用那条会过滤非法参数的判据。
        let delivered = final_answer_call(&calls);

        // 收尾轮没有下一轮可以重试，任何拿不到合法 final_answer 的情形都必须软着陆，
        // 否则整个 run 会报错，把此前所有工具结果一起丢掉。
        let answer = match delivered {
            Some(func) => match final_answer::extract_answer(&func.function.arguments) {
                Ok(answer) => answer,
                Err(error) => {
                    tracing::warn!(%error, "收尾轮 final_answer 参数非法，退回 content/兜底文案");
                    best_effort_answer(&reply.content)
                }
            },
            None => {
                tracing::warn!("端点无视 tool_choice 具名强制，收尾轮退回 content/兜底文案");
                best_effort_answer(&reply.content)
            }
        };

        // 先落助手消息，再补齐每个 call 的配对 tool 消息，保证历史结构合法。
        self.history.assistant(&reply.content, calls.clone())?;
        if let Some(func) = delivered {
            self.pair_delivery(context, &calls, func, &answer)?;
        }

        context.push_event(Event::new(
            EventName::Answer,
            answer.clone(),
            Role::Assistant,
        ));
        on_step(&Step::Answer {
            turn,
            content: answer.clone(),
        });

        Ok(answer)
    }
}

/// 兄弟调用在交付路径上的占位回填：它们从未被执行，但每个 `tool_call` 都必须有
/// 配对的 tool 消息，否则历史结构非法、无法再次发送。
const SKIPPED_CALL_NOTE: &str = "本轮已由 final_answer 结束，该调用未执行。";

/// 收尾轮追加的 system 指令：既讲清「必须交付」，也允许模型承认信息缺失，
/// 免得它为了凑一个确定答案而编造。
const FINALIZE_INSTRUCTION: &str = "你已经达到最大工具调用轮次。请基于当前已收集的全部信息，立即调用 \
     final_answer 工具给出你能提供的最佳答案。如果信息不足，请在 answer 中明确说明哪些信息缺失。";

/// 收尾轮连 content 都拿不到时的最后兜底：宁可交付一句可读的说明，也不让整个 run
/// 因收尾失败而报错，丢掉此前所有工具结果。
const FINALIZE_EMPTY_FALLBACK: &str =
    "已达到最大工具调用轮次，但模型未能给出最终答案；请基于已有信息重新提问或调整问题。";

/// 过滤出本仓库能执行、能配对的 function 调用。
///
/// `Custom` 等变体既没有执行路径，也无法回填 tool 消息——写进历史只会留下无人
/// 应答的 `tool_call`，所以在这里就丢掉并告警。
fn function_calls(
    tool_calls: &[ChatCompletionMessageToolCalls],
) -> Vec<ChatCompletionMessageToolCalls> {
    let calls: Vec<_> = tool_calls
        .iter()
        .filter(|call| matches!(call, ChatCompletionMessageToolCalls::Function(_)))
        .cloned()
        .collect();

    if calls.len() != tool_calls.len() {
        tracing::warn!(
            "忽略 {} 个非 function 类型的工具调用（无法执行，也无法回填）",
            tool_calls.len() - calls.len()
        );
    }

    calls
}

/// 收尾轮的工具面：只留 `final_answer`，让模型没有旁路可走。
///
/// 返回 `None` 表示工具表里没有 `final_answer`（违反 [`ReactLoop::new`] 的约定）：
/// 此时退化成不带工具的请求，由 `finalize` 的兜底分支接管，而不是构造出非法的
/// 具名 `tool_choice`。
fn finalize_tools(tools: &ToolHashMap) -> Option<ToolHashMap> {
    tools
        .get(FINAL_ANSWER_TOOL)
        .map(|tool| ToolHashMap::from([(FINAL_ANSWER_TOOL.to_owned(), tool.clone())]))
}

/// 收尾轮拿不到合法 `final_answer` 时的软着陆：有 content 就用 content，没有就交付
/// 固定文案。绝不返回 `Err`——收尾轮失败会把整个 run 的已收集信息一起丢掉。
fn best_effort_answer(content: &str) -> String {
    if content.trim().is_empty() {
        FINALIZE_EMPTY_FALLBACK.to_owned()
    } else {
        content.to_owned()
    }
}

/// 按名字找出本轮里的 `final_answer` 调用（不校验参数）。
///
/// `run_turn` 与 `finalize` 都需要「本轮有没有 final_answer」这一判断，区别只在
/// 参数非法怎么办：循环内压成 Observation 重试，收尾轮必须照样配对消息。把纯匹配
/// 抽出来共用，免得两边各写一份 `match`。
fn final_answer_call(
    calls: &[ChatCompletionMessageToolCalls],
) -> Option<&ChatCompletionMessageToolCall> {
    calls.iter().find_map(|call| match call {
        ChatCompletionMessageToolCalls::Function(func)
            if func.function.name == FINAL_ANSWER_TOOL =>
        {
            Some(func)
        }
        _ => None,
    })
}

/// 找出本轮里参数合法的 `final_answer`：交付与否只看它，与遍历顺序无关。
fn find_final_answer(
    calls: &[ChatCompletionMessageToolCalls],
) -> Option<(&ChatCompletionMessageToolCall, String)> {
    let func = final_answer_call(calls)?;
    let answer = final_answer::extract_answer(&func.function.arguments).ok()?;
    Some((func, answer))
}

fn answer(
    content: String,
    turn: usize,
    context: &mut ExecuteContext,
    on_step: &mut impl FnMut(&Step),
    termination: Termination,
) -> Option<Outcome> {
    on_step(&Step::Answer {
        turn,
        content: content.clone(),
    });
    context.push_event(Event::new(
        EventName::Answer,
        content.clone(),
        Role::Assistant,
    ));
    Some(Outcome {
        answer: content,
        turns: turn,
        termination,
    })
}

fn observe(context: &ExecuteContext) {
    let payload = serde_json::json!({
        "id": context.id().to_string(),
        "events": context.events(),
    });

    match serde_json::to_string_pretty(&payload) {
        Ok(json) => tracing::info!("execute context\n{json}"),
        Err(error) => tracing::warn!(%error, "execute context 序列化失败"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionRequestToolMessageContent,
        ChatCompletionRequestUserMessageArgs, FunctionCall,
    };
    use serde_json::{Value, json};

    use crate::agent::llm::callback::{Callback, CallbackCompleter, CallbackEvent};
    use crate::agent::llm::models::{Reply, ToolPolicy};
    use crate::agent::react::approval::{ApprovalRequest, Confirmer, Decision};
    use crate::agent::react::models::DEFAULT_MAX_TURNS;
    use crate::settings::{ApprovalAction, ApprovalPolicy, ApprovalRule};
    use crate::tools::local::final_answer::{FINAL_ANSWER_TOOL, FinalAnswer};
    use crate::tools::tool::Tool;

    /// 纯文本回复：在 required 语义下这是「端点无视 tool_choice」的降级输入。
    fn text_reply(text: &str) -> Reply {
        Reply {
            content: text.to_owned(),
            tool_calls: Vec::new(),
        }
    }

    fn call_id(id: &str, name: &str, arguments: &str) -> Reply {
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

    fn call(name: &str, arguments: &str) -> Reply {
        call_id("call_1", name, arguments)
    }

    fn multi_call(calls: Vec<(&str, &str, &str)>) -> Reply {
        Reply {
            content: String::new(),
            tool_calls: calls
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
                .collect(),
        }
    }

    fn calls_echo(arguments: &str) -> Reply {
        call("echo", arguments)
    }

    fn calls_unknown() -> Reply {
        call("nope", "{}")
    }

    fn calls_final_answer(text: &str) -> Reply {
        call(FINAL_ANSWER_TOOL, &json!({ "answer": text }).to_string())
    }

    fn thinking_call(content: &str, name: &str, arguments: &str) -> Reply {
        let mut reply = call(name, arguments);
        reply.content = content.to_owned();
        reply
    }

    fn trace(steps: &RefCell<Vec<Step>>) -> Vec<(usize, &'static str)> {
        steps
            .borrow()
            .iter()
            .map(|step| match step {
                Step::Thought { turn, .. } => (*turn, "thought"),
                Step::Answer { turn, .. } => (*turn, "answer"),
                Step::Action { turn, .. } => (*turn, "action"),
                Step::Observation { turn, .. } => (*turn, "observation"),
            })
            .collect()
    }

    struct ScriptedCompleter {
        replies: Mutex<Vec<Reply>>,
        policies: Mutex<Vec<ToolPolicy>>,
        /// 每次请求实际暴露的工具名（排序后），用来断言收尾轮的裁剪。
        tool_names: Mutex<Vec<Vec<String>>>,
        /// 每次请求实际收到的消息（已过回调链），用来钉住「线上 ≠ 历史」。
        messages: Mutex<Vec<Vec<ChatCompletionRequestMessage>>>,
    }

    impl ScriptedCompleter {
        fn new(replies: Vec<Reply>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
                policies: Mutex::new(Vec::new()),
                tool_names: Mutex::new(Vec::new()),
                messages: Mutex::new(Vec::new()),
            })
        }

        fn next(
            &self,
            messages: &[ChatCompletionRequestMessage],
            tools: Option<&ToolHashMap>,
            policy: &ToolPolicy,
            on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
        ) -> anyhow::Result<Reply> {
            self.policies.lock().expect("锁被毒化").push(policy.clone());
            self.messages
                .lock()
                .expect("锁被毒化")
                .push(messages.to_vec());

            let mut names: Vec<String> = tools
                .map(|tools| tools.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            self.tool_names.lock().expect("锁被毒化").push(names);

            let mut replies = self.replies.lock().expect("锁被毒化");
            if replies.is_empty() {
                anyhow::bail!("预置响应已用尽");
            }
            let reply = replies.remove(0);
            if !reply.content.is_empty() {
                on_token(&reply.content);
            }
            Ok(reply)
        }

        fn policies(&self) -> Vec<ToolPolicy> {
            self.policies.lock().expect("锁被毒化").clone()
        }

        fn tool_names(&self) -> Vec<Vec<String>> {
            self.tool_names.lock().expect("锁被毒化").clone()
        }

        fn messages(&self) -> Vec<Vec<ChatCompletionRequestMessage>> {
            self.messages.lock().expect("锁被毒化").clone()
        }
    }

    #[async_trait::async_trait]
    impl Completer for ScriptedCompleter {
        async fn complete(
            &self,
            messages: &[ChatCompletionRequestMessage],
            tools: Option<&ToolHashMap>,
            policy: ToolPolicy,
        ) -> anyhow::Result<Reply> {
            self.next(messages, tools, &policy, &mut |_| {})
        }

        async fn stream(
            &self,
            messages: &[ChatCompletionRequestMessage],
            tools: Option<&ToolHashMap>,
            policy: ToolPolicy,
            on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
        ) -> anyhow::Result<Reply> {
            self.next(messages, tools, &policy, on_token)
        }
    }

    /// 执行次数探针：用来断言「不该执行」的调用确实没有执行。
    #[derive(Default)]
    struct EchoProbe(AtomicUsize);

    impl EchoProbe {
        fn count(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    struct EchoTool {
        executed: Arc<EchoProbe>,
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "回显输入。参数里含 boom 时报错。"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
            self.executed.0.fetch_add(1, Ordering::Relaxed);
            if args_json.contains("boom") {
                anyhow::bail!("工具内部炸了");
            }
            Ok(format!("echo:{args_json}"))
        }
    }

    /// 脚本化确认方：按预置队列依次给出决策，同时记录调用次数与请求内容。
    struct ScriptedConfirmer {
        decisions: Mutex<VecDeque<Decision>>,
        calls: AtomicUsize,
        requests: Mutex<Vec<ApprovalRequest>>,
    }

    impl ScriptedConfirmer {
        fn new(decisions: Vec<Decision>) -> Arc<Self> {
            Arc::new(Self {
                decisions: Mutex::new(decisions.into()),
                calls: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }

        fn requests(&self) -> Vec<ApprovalRequest> {
            self.requests.lock().expect("锁被毒化").clone()
        }
    }

    #[async_trait::async_trait]
    impl Confirmer for ScriptedConfirmer {
        async fn confirm(&self, request: &ApprovalRequest) -> Decision {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.requests
                .lock()
                .expect("锁被毒化")
                .push(request.clone());
            self.decisions
                .lock()
                .expect("锁被毒化")
                .pop_front()
                .expect("预置确认决策已用尽")
        }
    }

    /// 自动批准：非交互场景（评测批处理等）的假实现样板——策略仍可决定要问哪些工具，
    /// 但没人可问时不 fail-closed。放在测试里供拷贝，库本身不提供。
    struct AutoApprove;

    #[async_trait::async_trait]
    impl Confirmer for AutoApprove {
        async fn confirm(&self, _request: &ApprovalRequest) -> Decision {
            Decision::Approve
        }
    }

    fn build(completer: Arc<dyn Completer>) -> ReactLoop {
        build_with_probe(completer, DEFAULT_MAX_TURNS).0
    }

    fn build_with_max_turns(completer: Arc<dyn Completer>, max_turns: usize) -> ReactLoop {
        build_with_probe(completer, max_turns).0
    }

    /// 与 [`build`] 相同，但额外返回 echo 的执行次数探针。
    fn build_with_probe(
        completer: Arc<dyn Completer>,
        max_turns: usize,
    ) -> (ReactLoop, Arc<EchoProbe>) {
        let executed = Arc::new(EchoProbe::default());
        let mut tools = ToolHashMap::new();
        tools.insert(
            "echo".to_owned(),
            Arc::new(EchoTool {
                executed: executed.clone(),
            }) as Arc<dyn Tool>,
        );
        // 手工拼表同样要显式注册 final_answer：收尾轮会以 tool_choice 具名强制它。
        tools.insert(
            FINAL_ANSWER_TOOL.to_owned(),
            Arc::new(FinalAnswer) as Arc<dyn Tool>,
        );
        let agent = ReactLoop::new(completer, tools, "你是测试助手。", max_turns)
            .expect("构造 ReactLoop 失败");
        (agent, executed)
    }

    /// 只对匹配 `pattern` 的工具判 `ask` 的策略，其余全放行。
    fn ask_for(pattern: &str) -> ApprovalPolicy {
        ApprovalPolicy {
            rules: vec![ApprovalRule {
                pattern: pattern.to_owned(),
                action: ApprovalAction::Ask,
            }],
            ..Default::default()
        }
    }

    /// 与 [`build_with_probe`] 相同，但注入审批策略与（可选的）确认方。
    fn build_with_gate(
        completer: Arc<dyn Completer>,
        policy: ApprovalPolicy,
        confirmer: Option<Arc<dyn Confirmer>>,
    ) -> (ReactLoop, Arc<EchoProbe>) {
        let (agent, executed) = build_with_probe(completer, DEFAULT_MAX_TURNS);
        let mut agent = agent.with_approval_policy(policy);
        if let Some(confirmer) = confirmer {
            agent = agent.with_confirmer(confirmer);
        }
        (agent, executed)
    }

    fn tool_messages(agent: &ReactLoop) -> usize {
        agent
            .history()
            .iter()
            .filter(|m| matches!(m, ChatCompletionRequestMessage::Tool(_)))
            .count()
    }

    /// 构造一条 user 消息，供注入回调使用。
    fn user_message(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestUserMessageArgs::default()
            .content(text)
            .build()
            .expect("构造 user 消息失败")
            .into()
    }

    /// 把消息序列化成 JSON 文本做包含判断。
    fn message_text(message: &ChatCompletionRequestMessage) -> String {
        serde_json::to_string(message).expect("序列化消息失败")
    }

    /// 在 `BeforeSend` 往尾部追加一条注入消息的回调。
    struct InjectMessage(&'static str);

    #[async_trait::async_trait]
    impl Callback for InjectMessage {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            if let CallbackEvent::BeforeSend { messages } = event {
                messages.push(user_message(self.0));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn final_answer_terminates_with_its_argument() {
        // final_answer 不被执行：参数即答案（`extract_answer` 与 `execute` 同源，
        // 所以这里断言的值与旧的 execute 返回值一致）。
        let mut agent = build(ScriptedCompleter::new(vec![calls_final_answer("42")]));

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("应当成功");

        assert_eq!(outcome.answer, "42");
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(
            tool_messages(&agent),
            1,
            "交付也必须留下配对 tool 消息，历史才可重放"
        );
    }

    #[tokio::test]
    async fn text_only_reply_degrades_to_answer() {
        // required 下服务端必须给 tool_call；纯文本说明端点无视了 tool_choice。
        let completer = ScriptedCompleter::new(vec![text_reply("答案是 42")]);
        let mut agent = build(completer.clone());

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("应当成功");

        assert_eq!(outcome.answer, "答案是 42");
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.termination, Termination::ModelFinished);
        assert_eq!(
            completer.policies(),
            vec![ToolPolicy::Required],
            "循环内一律 required"
        );
    }

    #[tokio::test]
    async fn executes_tool_then_answers() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            calls_final_answer("工具结果如上"),
        ]));

        let outcome = agent
            .run("用工具", |_| {}, |_, _| {})
            .await
            .expect("应当成功");

        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.answer, "工具结果如上");
        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(
            tool_messages(&agent),
            2,
            "echo 与 final_answer 各有一条配对 tool 消息"
        );
    }

    #[tokio::test]
    async fn tool_failure_becomes_observation() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"boom"}"#),
            calls_final_answer("工具失败了"),
        ]));

        let outcome = agent
            .run("用工具", |_| {}, |_, _| {})
            .await
            .expect("工具失败不应中断循环");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(tool_messages(&agent), 2, "失败的调用同样要回填 tool 消息");
    }

    #[tokio::test]
    async fn unknown_tool_becomes_observation() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_unknown(),
            calls_final_answer("没有这个工具"),
        ]));

        let outcome = agent
            .run("用工具", |_| {}, |_, _| {})
            .await
            .expect("未知工具不应中断循环");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(tool_messages(&agent), 2);
    }

    #[tokio::test]
    async fn invalid_final_answer_becomes_observation_and_retries() {
        let mut agent = build(ScriptedCompleter::new(vec![
            call(FINAL_ANSWER_TOOL, "not json"),
            calls_final_answer("补上的答案"),
        ]));

        let observed = RefCell::new(Vec::new());
        let outcome = agent
            .run(
                "问题",
                |step| observed.borrow_mut().push(step.clone()),
                |_, _| {},
            )
            .await
            .expect("参数非法不应中断循环");

        assert_eq!(outcome.turns, 2, "第一轮不该被当成终止");
        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(outcome.answer, "补上的答案");
        assert!(
            observed.borrow().iter().any(|step| matches!(
                step,
                Step::Observation { name, output, .. }
                    if name == FINAL_ANSWER_TOOL && output.contains("工具执行失败")
            )),
            "参数错误必须压成 Observation，实际轨迹：{:?}",
            trace(&observed)
        );
    }

    #[tokio::test]
    async fn final_answer_alongside_other_calls_still_pairs_every_tool_message() {
        let completer = ScriptedCompleter::new(vec![multi_call(vec![
            ("call_1", "echo", r#"{"q":"hi"}"#),
            (
                "call_2",
                FINAL_ANSWER_TOOL,
                r#"{"answer":"并存时以 final_answer 为准"}"#,
            ),
        ])]);
        let (mut agent, executed) = build_with_probe(completer, DEFAULT_MAX_TURNS);

        let observed = RefCell::new(Vec::new());
        let outcome = agent
            .run(
                "问题",
                |step| observed.borrow_mut().push(step.clone()),
                |_, _| {},
            )
            .await
            .expect("应当成功");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(outcome.answer, "并存时以 final_answer 为准");
        assert_eq!(executed.count(), 0, "答案已定，同轮其它调用一律不执行");
        assert_eq!(tool_messages(&agent), 2, "两个 call 都要有配对 tool 消息");
        assert_eq!(
            trace(&observed),
            vec![(1, "answer")],
            "交付不是工具回合：不执行、不发 Action/Observation"
        );

        let tool_texts: Vec<String> = agent
            .history()
            .iter()
            .filter_map(|m| match m {
                ChatCompletionRequestMessage::Tool(tool) => match &tool.content {
                    ChatCompletionRequestToolMessageContent::Text(text) => Some(text.clone()),
                    ChatCompletionRequestToolMessageContent::Array(_) => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_texts,
            vec![
                SKIPPED_CALL_NOTE.to_owned(),
                "并存时以 final_answer 为准".to_owned()
            ],
            "被跳过的调用回填占位文本，交付的那条回填答案"
        );
    }

    #[tokio::test]
    async fn invalid_final_answer_among_siblings_does_not_terminate() {
        // 参数非法 → 本轮不算交付：兄弟调用照常执行并配对，让模型下一轮重试。
        let completer = ScriptedCompleter::new(vec![
            multi_call(vec![
                ("call_1", "echo", r#"{"q":"hi"}"#),
                ("call_2", FINAL_ANSWER_TOOL, "not json"),
            ]),
            calls_final_answer("补上的答案"),
        ]);
        let (mut agent, executed) = build_with_probe(completer, DEFAULT_MAX_TURNS);

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("参数非法不应中断循环");

        assert_eq!(outcome.turns, 2, "第一轮不该被当成终止");
        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(outcome.answer, "补上的答案");
        assert_eq!(executed.count(), 1, "本轮仍有真实调用，应照常执行");
        assert_eq!(
            tool_messages(&agent),
            3,
            "第一轮两个 call 各一条 + 交付轮一条，历史全程可重放"
        );
    }

    #[tokio::test]
    async fn max_turns_still_produces_an_answer() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"1"}"#),
            calls_echo(r#"{"q":"2"}"#),
            calls_final_answer("被迫收尾的答案"),
        ]);
        let mut agent = build_with_max_turns(completer.clone(), 2);

        let observed = RefCell::new(Vec::new());
        let outcome = agent
            .run(
                "一直用工具",
                |step| observed.borrow_mut().push(step.clone()),
                |_, _| {},
            )
            .await
            .expect("撞上限也应拿到答案");

        assert_eq!(outcome.termination, Termination::MaxTurns);
        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.answer, "被迫收尾的答案");
        assert_eq!(
            tool_messages(&agent),
            3,
            "两轮 echo + 收尾轮的 final_answer"
        );
        assert_eq!(
            completer.policies(),
            vec![
                ToolPolicy::Required,
                ToolPolicy::Required,
                ToolPolicy::Force(FINAL_ANSWER_TOOL.to_owned()),
            ],
            "循环内 required，收尾轮具名强制 final_answer"
        );
        assert!(
            observed.borrow().iter().any(|step| matches!(
                step,
                Step::Answer { content, .. } if content == "被迫收尾的答案"
            )),
            "收尾轮也必须发 Step::Answer，实际轨迹：{:?}",
            trace(&observed)
        );
    }

    #[tokio::test]
    async fn finalize_turn_only_exposes_final_answer() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"1"}"#),
            calls_final_answer("裁剪后的答案"),
        ]);
        let mut agent = build_with_max_turns(completer.clone(), 1);

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("应当成功");

        assert_eq!(outcome.answer, "裁剪后的答案");
        assert_eq!(
            completer.tool_names(),
            vec![
                vec!["echo".to_owned(), FINAL_ANSWER_TOOL.to_owned()],
                vec![FINAL_ANSWER_TOOL.to_owned()],
            ],
            "收尾轮只应暴露 final_answer，其它工具不再给模型留旁路"
        );
    }

    #[tokio::test]
    async fn finalize_falls_back_to_content_when_force_is_ignored() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"1"}"#),
            text_reply("收尾轮只能说这些"),
        ]);
        let mut agent = build_with_max_turns(completer, 1);

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("端点忽略强制也应软着陆");

        assert_eq!(outcome.termination, Termination::MaxTurns);
        assert_eq!(outcome.answer, "收尾轮只能说这些");
    }

    #[tokio::test]
    async fn finalize_with_empty_reply_still_delivers_a_message() {
        let completer = ScriptedCompleter::new(vec![calls_echo(r#"{"q":"1"}"#), Reply::default()]);
        let mut agent = build_with_max_turns(completer, 1);

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("收尾轮空响应不应让整个 run 报错");

        assert_eq!(outcome.termination, Termination::MaxTurns);
        assert_eq!(outcome.answer, FINALIZE_EMPTY_FALLBACK);
    }

    #[tokio::test]
    async fn finalize_with_invalid_answer_degrades_instead_of_failing() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"1"}"#),
            call(FINAL_ANSWER_TOOL, "not json"),
        ]);
        let mut agent = build_with_max_turns(completer, 1);

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("收尾轮参数非法也应软着陆");

        assert_eq!(outcome.termination, Termination::MaxTurns);
        assert_eq!(outcome.answer, FINALIZE_EMPTY_FALLBACK);
        assert_eq!(
            tool_messages(&agent),
            2,
            "echo 与非法 final_answer 都要配对"
        );
    }

    #[tokio::test]
    async fn final_turn_is_an_answer_not_a_thought() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            calls_final_answer("最终答案"),
        ]));

        let observed = RefCell::new(Vec::new());
        let outcome = agent
            .run(
                "问题",
                |step| observed.borrow_mut().push(step.clone()),
                |_, _| {},
            )
            .await
            .expect("应当成功");

        assert_eq!(outcome.answer, "最终答案");
        assert_eq!(
            trace(&observed),
            vec![(1, "action"), (1, "observation"), (2, "answer")],
            "交付轮只有 Answer：不执行工具，因此没有 action/observation"
        );
    }

    #[tokio::test]
    async fn intermediate_turn_with_content_is_a_thought() {
        let mut agent = build(ScriptedCompleter::new(vec![
            thinking_call("我先查一下", "echo", r#"{"q":"hi"}"#),
            calls_final_answer("答案"),
        ]));

        let observed = RefCell::new(Vec::new());
        agent
            .run(
                "问题",
                |step| observed.borrow_mut().push(step.clone()),
                |_, _| {},
            )
            .await
            .expect("应当成功");

        assert_eq!(
            trace(&observed),
            vec![
                (1, "thought"),
                (1, "action"),
                (1, "observation"),
                (2, "answer"),
            ],
            "第一轮有 content 且有 tool_calls，应发 Thought 而非 Answer"
        );
    }

    #[tokio::test]
    async fn empty_reply_ends_the_loop_with_an_empty_message() {
        let mut agent = build(ScriptedCompleter::new(vec![Reply::default()]));

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("空回复应当收束循环");

        assert_eq!(outcome.answer, "");
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.termination, Termination::EmptyReply);

        let last = agent.history().last().expect("历史至少有一条消息");
        assert!(
            matches!(last, ChatCompletionRequestMessage::Assistant(message) if message.content.is_none()),
            "空回复要在历史末尾留下一条空的 assistant 消息"
        );
    }

    #[tokio::test]
    async fn execute_flattens_failures_into_observations() {
        let agent = build(ScriptedCompleter::new(Vec::new()));

        assert_eq!(
            agent.execute("echo", r#"{"q":"hi"}"#).await,
            r#"echo:{"q":"hi"}"#
        );

        let failed = agent.execute("echo", r#"{"q":"boom"}"#).await;
        assert!(failed.contains("工具执行失败"));

        assert_eq!(agent.execute("nope", "{}").await, "未知工具：nope");
        assert_eq!(
            agent
                .execute(FINAL_ANSWER_TOOL, r#"{"answer":"输入即输出"}"#)
                .await,
            "输入即输出"
        );
    }

    #[tokio::test]
    async fn denied_call_becomes_observation_and_loop_continues() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            calls_final_answer("被拒之后改口"),
        ]);
        let confirmer = ScriptedConfirmer::new(vec![Decision::Deny]);
        let (mut agent, executed) =
            build_with_gate(completer, ask_for("echo"), Some(confirmer.clone()));

        let observed = RefCell::new(Vec::new());
        let outcome = agent
            .run(
                "问题",
                |step| observed.borrow_mut().push(step.clone()),
                |_, _| {},
            )
            .await
            .expect("拒绝不应中断循环");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(outcome.answer, "被拒之后改口");
        assert_eq!(executed.count(), 0, "被拒的调用不得执行");
        assert_eq!(confirmer.call_count(), 1);
        assert_eq!(
            tool_messages(&agent),
            2,
            "被拒的调用同样要回填配对 tool 消息"
        );
        assert_eq!(
            trace(&observed),
            vec![(1, "action"), (1, "observation"), (2, "answer")],
            "被拒是「尝试了、被拒了」：先 Action、后 Observation，无需新事件类型"
        );

        let denied = observed
            .borrow()
            .iter()
            .find_map(|step| match step {
                Step::Observation { output, .. } => Some(output.clone()),
                _ => None,
            })
            .expect("被拒的调用应产生一条 Observation");
        assert!(denied.contains("拒绝"), "观察文案应讲明被拒：{denied}");

        let requests = confirmer.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].turn, 1);
        assert_eq!(requests[0].tool, "echo");
        assert_eq!(
            requests[0].description, "回显输入。参数里含 boom 时报错。",
            "确认请求应带上工具的 description"
        );
    }

    #[tokio::test]
    async fn approved_call_executes_normally() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            calls_final_answer("答案"),
        ]);
        let (mut agent, executed) =
            build_with_gate(completer, ask_for("echo"), Some(Arc::new(AutoApprove)));

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("批准后应照常执行");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(executed.count(), 1, "批准后照常执行");
    }

    #[tokio::test]
    async fn ask_without_confirmer_is_denied() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            calls_final_answer("退而求其次"),
        ]);
        let (mut agent, executed) = build_with_gate(completer, ask_for("echo"), None);

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("fail-closed 之后也应正常收尾");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(executed.count(), 0, "策略要问但无人可问时不得执行");
        assert_eq!(tool_messages(&agent), 2);
    }

    #[tokio::test]
    async fn allow_policy_never_consults_confirmer() {
        let completer = ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            calls_final_answer("答案"),
        ]);
        // 策略全放行；confirmer 预置了拒绝决策——若被咨询，执行次数会变成 0。
        let confirmer = ScriptedConfirmer::new(vec![Decision::Deny]);
        let (mut agent, executed) = build_with_gate(
            completer,
            ApprovalPolicy::default(),
            Some(confirmer.clone()),
        );

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("默认策略应全放行");

        assert_eq!(outcome.termination, Termination::FinalAnswer);
        assert_eq!(executed.count(), 1, "放行的调用照常执行");
        assert_eq!(confirmer.call_count(), 0, "allow 不必咨询 confirmer");
    }

    #[tokio::test]
    async fn before_send_injection_reaches_transport_but_not_history() {
        const INJECTED: &str = "【注入】动态上下文（不应落历史）";

        let scripted = ScriptedCompleter::new(vec![calls_final_answer("完成")]);
        let completer: Arc<dyn Completer> = Arc::new(CallbackCompleter::new(
            scripted.clone(),
            vec![Arc::new(InjectMessage(INJECTED))],
        ));
        let mut agent = build(completer);

        let outcome = agent
            .run("原始任务", |_| {}, |_, _| {})
            .await
            .expect("run 失败");
        assert_eq!(outcome.answer, "完成");

        let seen = scripted.messages();
        assert_eq!(seen.len(), 1, "只有一轮请求");
        assert!(
            seen[0].iter().any(|m| message_text(m).contains(INJECTED)),
            "注入的消息应到达传输层"
        );
        assert!(
            !agent
                .history()
                .iter()
                .any(|m| message_text(m).contains(INJECTED)),
            "注入的消息不应写进 History（线上 ≠ 存档）"
        );
    }
}
