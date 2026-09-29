use std::sync::Arc;

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, ChatCompletionRequestMessage,
    FunctionCall,
};
use tracing::info;

use crate::agent::llm::models::{Completer, Reply};
use crate::agent::react::context::{Event, EventName, ExecuteContext, Role, Status};
use crate::agent::react::history::History;
use crate::agent::react::models::{Outcome, Step, Termination};
use crate::tools::ToolHashMap;

pub struct ReactLoop {
    completer: Arc<dyn Completer>,
    tools: ToolHashMap,
    history: History,
    max_turns: usize,
}

impl ReactLoop {
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
        })
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
        let answer = self.finalize(context, on_token).await?;
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

        if content.is_empty() && tool_calls.is_empty() {
            self.history.assistant(&content, tool_calls)?;
            return Ok(Some(Outcome {
                answer: content,
                turns: turn,
                termination: Termination::EmptyReply,
            }));
        }

        self.history.assistant(&content, tool_calls.clone())?;

        if tool_calls.is_empty() {
            return Ok(answer(content, turn, context, on_step));
        }

        for call in &tool_calls {
            let ChatCompletionMessageToolCalls::Function(func_call) = call else {
                continue;
            };
            let func = &func_call.function;
            let observation = self.action(func, turn, context, on_step).await;
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
            .stream(self.history.as_slice(), Some(&self.tools), &mut |token| {
                on_token(turn, token)
            })
            .await?;

        // content非空, tool_calls非空 -> content = thought
        // content非空, tool_calls为空 -> content = answer
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
        on_token: &mut (impl FnMut(usize, &str) + Send),
    ) -> anyhow::Result<String> {
        self.history
            .user("请基于以上信息给出最终回答，不要再调用任何工具。")?;

        let turn = self.max_turns + 1;
        context.set_turn(turn);

        let reply = self
            .completer
            .stream(self.history.as_slice(), None, &mut |token| {
                on_token(turn, token)
            })
            .await?;
        let answer = reply.content;

        if answer.trim().is_empty() {
            anyhow::bail!("收尾轮返回了空内容");
        }
        context.push_event(Event::new(
            EventName::Answer,
            answer.clone(),
            Role::Assistant,
        ));
        self.history.assistant(&answer, Vec::new())?;
        Ok(answer)
    }
}

fn answer(
    content: String,
    turn: usize,
    context: &mut ExecuteContext,
    on_step: &mut impl FnMut(&Step),
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
        termination: Termination::ModelFinished,
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
    use std::sync::Mutex;

    use async_openai::types::chat::{ChatCompletionMessageToolCall, FunctionCall};
    use serde_json::{Value, json};

    use crate::agent::llm::models::Reply;
    use crate::agent::react::models::DEFAULT_MAX_TURNS;
    use crate::tools::tool::Tool;

    fn answer(text: &str) -> Reply {
        Reply {
            content: text.to_owned(),
            tool_calls: Vec::new(),
        }
    }

    fn call(name: &str, arguments: &str) -> Reply {
        Reply {
            content: String::new(),
            tool_calls: vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: "call_1".to_owned(),
                    function: FunctionCall {
                        name: name.to_owned(),
                        arguments: arguments.to_owned(),
                    },
                },
            )],
        }
    }

    fn calls_echo(arguments: &str) -> Reply {
        call("echo", arguments)
    }

    fn calls_unknown() -> Reply {
        call("nope", "{}")
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
    }

    impl ScriptedCompleter {
        fn new(replies: Vec<Reply>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
            })
        }

        fn next(
            &self,
            on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
        ) -> anyhow::Result<Reply> {
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
    }

    #[async_trait::async_trait]
    impl Completer for ScriptedCompleter {
        async fn complete(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: Option<&ToolHashMap>,
        ) -> anyhow::Result<Reply> {
            self.next(&mut |_| {})
        }

        async fn stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: Option<&ToolHashMap>,
            on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
        ) -> anyhow::Result<Reply> {
            self.next(on_token)
        }
    }

    struct EchoTool;

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
            if args_json.contains("boom") {
                anyhow::bail!("工具内部炸了");
            }
            Ok(format!("echo:{args_json}"))
        }
    }

    fn build(completer: Arc<dyn Completer>) -> ReactLoop {
        build_with_max_turns(completer, DEFAULT_MAX_TURNS)
    }

    fn build_with_max_turns(completer: Arc<dyn Completer>, max_turns: usize) -> ReactLoop {
        let mut tools = ToolHashMap::new();
        tools.insert("echo".to_owned(), Arc::new(EchoTool) as Arc<dyn Tool>);
        ReactLoop::new(completer, tools, "你是测试助手。", max_turns).expect("构造 ReactLoop 失败")
    }

    fn tool_messages(agent: &ReactLoop) -> usize {
        agent
            .history()
            .iter()
            .filter(|m| matches!(m, ChatCompletionRequestMessage::Tool(_)))
            .count()
    }

    #[tokio::test]
    async fn answers_directly_without_tools() {
        let mut agent = build(ScriptedCompleter::new(vec![answer("答案是 42")]));

        let outcome = agent
            .run("问题", |_| {}, |_, _| {})
            .await
            .expect("应当成功");

        assert_eq!(outcome.answer, "答案是 42");
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.termination, Termination::ModelFinished);
    }

    #[tokio::test]
    async fn executes_tool_then_answers() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            answer("工具结果如上"),
        ]));

        let outcome = agent
            .run("用工具", |_| {}, |_, _| {})
            .await
            .expect("应当成功");

        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.answer, "工具结果如上");
        assert_eq!(
            tool_messages(&agent),
            1,
            "每个 tool_call 都要有配对的 tool 消息"
        );
    }

    #[tokio::test]
    async fn tool_failure_becomes_observation() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"boom"}"#),
            answer("工具失败了"),
        ]));

        let outcome = agent
            .run("用工具", |_| {}, |_, _| {})
            .await
            .expect("工具失败不应中断循环");

        assert_eq!(outcome.termination, Termination::ModelFinished);
        assert_eq!(tool_messages(&agent), 1, "失败的调用同样要回填 tool 消息");
    }

    #[tokio::test]
    async fn unknown_tool_becomes_observation() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_unknown(),
            answer("没有这个工具"),
        ]));

        let outcome = agent
            .run("用工具", |_| {}, |_, _| {})
            .await
            .expect("未知工具不应中断循环");

        assert_eq!(outcome.termination, Termination::ModelFinished);
        assert_eq!(tool_messages(&agent), 1);
    }

    #[tokio::test]
    async fn max_turns_still_produces_an_answer() {
        let mut agent = build_with_max_turns(
            ScriptedCompleter::new(vec![
                calls_echo(r#"{"q":"1"}"#),
                calls_echo(r#"{"q":"2"}"#),
                answer("被迫收尾的答案"),
            ]),
            2,
        );

        let outcome = agent
            .run("一直用工具", |_| {}, |_, _| {})
            .await
            .expect("撞上限也应拿到答案");

        assert_eq!(outcome.termination, Termination::MaxTurns);
        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.answer, "被迫收尾的答案");
    }

    #[tokio::test]
    async fn final_turn_is_an_answer_not_a_thought() {
        let mut agent = build(ScriptedCompleter::new(vec![
            calls_echo(r#"{"q":"hi"}"#),
            answer("最终答案"),
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
            vec![(1, "action"), (1, "observation"), (2, "answer"),],
            "收尾轮的 content 必须发 Answer，不能走 Thought"
        );
    }

    #[tokio::test]
    async fn intermediate_turn_with_content_is_a_thought() {
        let mut agent = build(ScriptedCompleter::new(vec![
            thinking_call("我先查一下", "echo", r#"{"q":"hi"}"#),
            answer("答案"),
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
    }
}
