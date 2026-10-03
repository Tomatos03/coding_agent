//! 端到端示例：用回调链在「请求前后」做观察掩蔽、窗口裁剪、动态注入与回复脱敏。
//!
//! 全程脚本化 `LLMClient` + 桩 `read_file` 工具，**无需任何凭证、可离线运行**。演示的
//! 回调链按注册顺序为 `[Logger, MaskStaleObservations, SlidingWindowTrim, InjectContext,
//! RedactReply, TransportLog]`：
//!
//! - `BeforeSend` **正序**依次跑；`AfterSend` **逆序**跑（洋葱模型），因此最外层的
//!   `Logger` 在 `AfterSend` 里看到的是脱敏后的最终回复。
//! - 脚本模型先两轮读文件（可重读，交给掩蔽），再三轮列目录（不可重读，只能靠裁剪
//!   兜底），最后交付答案。
//! - `MaskStaleObservations` 把旧的 `read_file` 结果打桩（一条消息 full→stub 一生一次）；
//!   `SlidingWindowTrim` 在超出 token 高水位时按完整调用组丢弃最旧的消息（兜底）；
//!   `InjectContext` 每轮在尾部追加动态上下文（不落历史）；`RedactReply` 改写回复里的
//!   敏感串。
//!
//! 运行：
//!   cargo run --example callback_react

use std::collections::HashMap;
use std::sync::Arc;

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, ChatCompletionRequestMessage,
    ChatCompletionRequestToolMessageArgs, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessageArgs, FunctionCall,
};
use coding_agent::agent::llm::callback::{Callback, CallbackEvent};
use coding_agent::agent::llm::models::{LLMClient, Reply};
use coding_agent::agent::react::models::{DEFAULT_MAX_TURNS, Step};
use coding_agent::agent::react::runner::ReactLoop;
use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::tools::ToolHashMap;
use coding_agent::tools::local::final_answer::{FINAL_ANSWER_TOOL, FinalAnswer};
use coding_agent::tools::tool::Tool;
use serde_json::{Value, json};

/// 桩工具：`read_file` 返回一段足够大的正文，让 token 预算的水位有意义。
struct ReadFileStub;

#[async_trait::async_trait]
impl Tool for ReadFileStub {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "读取文件内容（示例桩：返回固定大文本，便于演示裁剪）。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: Value = serde_json::from_str(args_json)?;
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        Ok(format!("// 文件 {path}\n{}", "let x = 1;\n".repeat(24)))
    }
}

/// 桩工具：`list_files`。它的结果**不可重读**（目录会变），因此掩蔽回调不会碰它，
/// 历史只能靠滑动窗口裁剪兜底。
struct ListFilesStub;

#[async_trait::async_trait]
impl Tool for ListFilesStub {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "列出目录条目（示例桩）。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        let args: Value = serde_json::from_str(args_json)?;
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        Ok(format!("{path}/\n{}", "src/main.rs\n".repeat(20)))
    }
}

/// 假装自己是传输层：在 `BeforeSend` 打印最终发出去的消息概览。
///
/// 注册在回调链最内层——`BeforeSend` 最后执行，因此看到的就是传输层实际收到的版本，
/// 用来证明前面的注入 / 打桩 / 裁剪确实送达了传输层。
struct TransportLog;

#[async_trait::async_trait]
impl Callback for TransportLog {
    async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
        if let CallbackEvent::BeforeSend { messages } = event {
            println!(
                "  [传输层] 收到 {} 条消息 | {}",
                messages.len(),
                summarize(messages)
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 回调实现
// ---------------------------------------------------------------------------

/// 审计：成对记录请求与响应。注册在最外层，因此 `AfterSend` 看到的是最终形态。
struct Logger;

#[async_trait::async_trait]
impl Callback for Logger {
    async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
        match event {
            CallbackEvent::BeforeSend { messages } => {
                println!(
                    "[回调] Logger.BeforeSend：{} 条消息（正序派发开始）",
                    messages.len()
                );
            }
            CallbackEvent::AfterSend { reply, .. } => {
                println!(
                    "[回调] Logger.AfterSend：最终回复 = {:?}（逆序派发结束，日志记到脱敏后的形态）",
                    reply.content
                );
            }
            // `CallbackEvent` 标了 `#[non_exhaustive]`：新增变体时这里无需改动。
            _ => {}
        }
        Ok(())
    }
}

/// 观察掩蔽：把旧的 `read_file` 结果替换成固定桩文本。
///
/// - **攒批**：可掩蔽的旧结果不足 `batch` 条时不动手，两次打桩之间请求保持 append-only。
/// - **单向 + 桩文本固定**：`path` 固定，因此同一份历史每轮派发结果一致、幂等。
struct MaskStaleObservations {
    /// 最新多少条 `read_file` 观察保持原文可读。
    keep: usize,
    /// 攒够多少条「待掩蔽」才触发一次打桩。
    batch: usize,
}

impl MaskStaleObservations {
    fn new(keep: usize, batch: usize) -> Self {
        Self { keep, batch }
    }
}

/// 桩文本只带文件路径（每条固定），不可带时间戳 / 轮次。
fn stub_text(path: &str) -> String {
    format!(
        "【已掩蔽】此处省略一次 read_file({path}) 的结果，需要时重新调用 read_file 读取当前内容。"
    )
}

fn tool_message(tool_call_id: &str, content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestToolMessageArgs::default()
        .tool_call_id(tool_call_id)
        .content(content)
        .build()
        .expect("构造 tool 消息失败")
        .into()
}

/// 从消息序列里反查 `tool_call_id -> (工具名, 参数)`。
fn collect_calls(messages: &[ChatCompletionRequestMessage]) -> HashMap<String, (String, String)> {
    let mut calls = HashMap::new();
    for message in messages {
        let ChatCompletionRequestMessage::Assistant(assistant) = message else {
            continue;
        };
        for call in assistant.tool_calls.iter().flatten() {
            let ChatCompletionMessageToolCalls::Function(function) = call else {
                continue;
            };
            calls.insert(
                function.id.clone(),
                (
                    function.function.name.clone(),
                    function.function.arguments.clone(),
                ),
            );
        }
    }
    calls
}

fn read_path(calls: &HashMap<String, (String, String)>, id: &str) -> String {
    calls
        .get(id)
        .and_then(|(_, args)| serde_json::from_str::<Value>(args).ok())
        .and_then(|value| value.get("path").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

#[async_trait::async_trait]
impl Callback for MaskStaleObservations {
    async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
        let CallbackEvent::BeforeSend { messages } = event else {
            return Ok(());
        };

        let calls = collect_calls(messages);
        // 找出所有 `read_file` 的观察消息（只有可重读的结果才能掩蔽）。
        let read_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                ChatCompletionRequestMessage::Tool(tool) => calls
                    .get(&tool.tool_call_id)
                    .filter(|(name, _)| name == "read_file")
                    .map(|_| index),
                _ => None,
            })
            .collect();

        if read_indices.len() <= self.keep {
            return Ok(());
        }
        let older = &read_indices[..read_indices.len() - self.keep];
        if older.len() < self.batch {
            println!(
                "[回调] MaskStaleObservations：待掩蔽 {} 条 < 批次 {}，暂不动手（保持 append-only）",
                older.len(),
                self.batch
            );
            return Ok(());
        }

        let mut masked = 0;
        for &index in older {
            // 先取出并算好替换内容，结束对 `messages` 的不可变借用后再写回。
            let (id, stub) = {
                let ChatCompletionRequestMessage::Tool(tool) = &messages[index] else {
                    continue;
                };
                let path = read_path(&calls, &tool.tool_call_id);
                let stub = stub_text(&path);
                let current = match &tool.content {
                    ChatCompletionRequestToolMessageContent::Text(text) => text.clone(),
                    ChatCompletionRequestToolMessageContent::Array(_) => String::new(),
                };
                if current == stub {
                    continue;
                }
                (tool.tool_call_id.clone(), stub)
            };
            messages[index] = tool_message(&id, &stub);
            masked += 1;
        }
        if masked > 0 {
            println!(
                "[回调] MaskStaleObservations：把 {masked} 条旧 read_file 结果打桩（保留最新 {} 条）",
                self.keep
            );
        }
        Ok(())
    }
}

/// 滑动窗口裁剪：按 token 预算丢弃最旧的调用组（兜底）。
///
/// 三条硬约束：只能按完整调用组切（切点落在非 Tool 消息上）、钉住 system + 首条 user、
/// 高/低水位滞回（超上限才裁、一次裁到下限）。
struct SlidingWindowTrim {
    high: usize,
    low: usize,
}

impl SlidingWindowTrim {
    fn new(high: usize, low: usize) -> Self {
        Self { high, low }
    }
}

/// 粗估单条消息的 token 数：序列化字符数 / 4 + 每条固定开销。
fn estimate_message(message: &ChatCompletionRequestMessage) -> usize {
    let chars = serde_json::to_string(message)
        .map(|json| json.chars().count())
        .unwrap_or(0);
    chars / 4 + 4
}

fn estimate_tokens(messages: &[ChatCompletionRequestMessage]) -> usize {
    messages.iter().map(estimate_message).sum()
}

/// 最后一个「assistant(tool_calls) + 其配对 tool 消息」组的起点；没有则退化成最后一条。
fn latest_group_start(messages: &[ChatCompletionRequestMessage]) -> usize {
    let mut start = messages.len().saturating_sub(1);
    for (index, message) in messages.iter().enumerate() {
        let ChatCompletionRequestMessage::Assistant(assistant) = message else {
            continue;
        };
        if assistant
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
        {
            start = index;
        }
    }
    start
}

#[async_trait::async_trait]
impl Callback for SlidingWindowTrim {
    async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
        let CallbackEvent::BeforeSend { messages } = event else {
            return Ok(());
        };

        let total = estimate_tokens(messages);
        if total <= self.high {
            println!(
                "[回调] SlidingWindowTrim：估算 {total} token ≤ 高水位 {}，未触发",
                self.high
            );
            return Ok(());
        }

        // 钉住头部：system(0) 与首条 user(1) 永不丢弃。
        const PIN: usize = 2;
        if messages.len() <= PIN {
            return Ok(());
        }

        let target_drop = total.saturating_sub(self.low);
        let mut dropped = 0usize;
        let mut cut = PIN;
        while cut < messages.len() && dropped < target_drop {
            dropped += estimate_message(&messages[cut]);
            cut += 1;
        }
        // 不能切进最新一组调用。
        cut = cut.min(latest_group_start(messages)).max(PIN);
        // 切点不能落在 Tool 消息上，否则后缀会以孤儿 tool 结果开头 → 服务端 400。
        while cut > PIN && matches!(messages[cut], ChatCompletionRequestMessage::Tool(_)) {
            cut -= 1;
        }

        if cut > PIN {
            let removed = cut - PIN;
            messages.drain(PIN..cut);
            println!(
                "[回调] SlidingWindowTrim：估算 {total} token > 高水位 {}，丢弃 {removed} 条旧消息（保留钉子 + 最新一组）",
                self.high
            );
        }
        Ok(())
    }
}

/// 动态上下文注入：每轮在**尾部**追加，插在开头 / 中间会破坏前缀缓存。
struct InjectContext(&'static str);

#[async_trait::async_trait]
impl Callback for InjectContext {
    async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
        if let CallbackEvent::BeforeSend { messages } = event {
            let message = ChatCompletionRequestUserMessageArgs::default()
                .content(self.0)
                .build()
                .expect("构造注入消息失败");
            messages.push(message.into());
            println!("[回调] InjectContext：尾部追加动态上下文（只影响本次请求）");
        }
        Ok(())
    }
}

/// 响应脱敏：改写回复内容与工具调用参数里的敏感串。`AfterSend` 的改动会落历史 / 交付。
struct RedactReply(&'static str);

#[async_trait::async_trait]
impl Callback for RedactReply {
    async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
        let CallbackEvent::AfterSend { reply, .. } = event else {
            return Ok(());
        };
        let mut touched = false;
        if reply.content.contains(self.0) {
            reply.content = reply.content.replace(self.0, "***REDACTED***");
            touched = true;
        }
        for call in &mut reply.tool_calls {
            let ChatCompletionMessageToolCalls::Function(function) = call else {
                continue;
            };
            if function.function.arguments.contains(self.0) {
                function.function.arguments = function
                    .function
                    .arguments
                    .replace(self.0, "***REDACTED***");
                touched = true;
            }
        }
        if touched {
            println!("[回调] RedactReply：改写回复里的敏感串（逆序第一个）");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 脚本与装配
// ---------------------------------------------------------------------------

/// 构造一条「思考 + 调用某工具」的回复。
fn tool_call_reply(id: &str, name: &str, arguments: Value) -> Reply {
    Reply {
        content: format!("我先调用 {name} 看看。"),
        tool_calls: vec![ChatCompletionMessageToolCalls::Function(
            ChatCompletionMessageToolCall {
                id: id.to_owned(),
                function: FunctionCall {
                    name: name.to_owned(),
                    arguments: arguments.to_string(),
                },
            },
        )],
    }
}

fn final_answer_reply(id: &str, answer: &str) -> Reply {
    Reply {
        content: String::new(),
        tool_calls: vec![ChatCompletionMessageToolCalls::Function(
            ChatCompletionMessageToolCall {
                id: id.to_owned(),
                function: FunctionCall {
                    name: FINAL_ANSWER_TOOL.to_owned(),
                    arguments: json!({ "answer": answer }).to_string(),
                },
            },
        )],
    }
}

/// 给出消息概览：是否含注入标记 / 打桩标记，供人眼核对。
fn summarize(messages: &[ChatCompletionRequestMessage]) -> String {
    let injected = messages
        .iter()
        .any(|m| format!("{m:?}").contains("【注入】"));
    let masked = messages
        .iter()
        .filter(|m| format!("{m:?}").contains("【已掩蔽】"))
        .count();
    format!("含注入={injected} 已打桩={masked}")
}

const SECRET: &str = "sk-SECRET-123456";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let mut tools = ToolHashMap::new();
    tools.insert(
        "read_file".to_owned(),
        Arc::new(ReadFileStub) as Arc<dyn Tool>,
    );
    tools.insert(
        "list_files".to_owned(),
        Arc::new(ListFilesStub) as Arc<dyn Tool>,
    );
    tools.insert(
        FINAL_ANSWER_TOOL.to_owned(),
        Arc::new(FinalAnswer) as Arc<dyn Tool>,
    );

    // 两轮读文件（可掩蔽）+ 三轮列目录（不可掩蔽，只能靠裁剪兜底）+ 一轮交付。
    let llm = LLMClient::scripted(vec![
        tool_call_reply("read_1", "read_file", json!({ "path": "a.rs" })),
        tool_call_reply("read_2", "read_file", json!({ "path": "b.rs" })),
        tool_call_reply("list_1", "list_files", json!({ "path": "." })),
        tool_call_reply("list_2", "list_files", json!({ "path": "src" })),
        tool_call_reply("list_3", "list_files", json!({ "path": "tests" })),
        final_answer_reply(
            "final_1",
            &format!("已读取两个文件并浏览目录。附带一个应被脱敏的令牌：{SECRET}"),
        ),
    ]);

    // 注册顺序 = `BeforeSend` 正序 / `AfterSend` 逆序。Logger 在最外层。
    let callbacks: Vec<Arc<dyn Callback>> = vec![
        Arc::new(Logger),
        Arc::new(MaskStaleObservations::new(1, 1)),
        Arc::new(SlidingWindowTrim::new(400, 200)),
        Arc::new(InjectContext(
            "【注入】当前时间与检索片段（每轮不同，不落历史）",
        )),
        Arc::new(RedactReply(SECRET)),
        // 最内层：BeforeSend 最后执行，看到的即传输层实际收到的版本。
        Arc::new(TransportLog),
    ];

    let llm: Arc<LLMClient> = Arc::new(llm.with_callbacks(callbacks));
    let mut agent = ReactLoop::new(llm, tools, SYSTEM_PROMPT, DEFAULT_MAX_TURNS)?;

    println!("用户问题：读取文件与目录，给出总结。\n");

    let outcome = agent
        .run(
            "请依次读取 a.rs、b.rs、c.rs 并总结。",
            &mut |step| match step {
                Step::Thought { turn, content } => println!("[{turn}] 思考：{content}"),
                Step::Answer { turn, content } => println!("\n[{turn}] 答案：{content}"),
                Step::Action {
                    turn,
                    name,
                    arguments,
                } => println!("[{turn}] 调用：{name} 参数 {arguments}"),
                Step::Observation { turn, name, output } => {
                    println!("[{turn}] {name} 返回 {} 字符", output.chars().count());
                }
            },
            &mut |_, _| {},
        )
        .await?;

    println!(
        "\n--- 终止于 {:?}，共 {} 轮 ---",
        outcome.termination, outcome.turns
    );
    println!(
        "答案中的敏感串已被改写为 `***REDACTED***`：{}",
        outcome.answer.contains("***REDACTED***")
    );

    Ok(())
}
