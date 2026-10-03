use super::callback::{self, Callback};
use super::provider;
use super::test_support::{Scripted, ScriptedRequest};
use crate::constant::provider::MAX_TOKENS_ENV;

use crate::tools::{ToolHashMap, tool_definitions};

use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestMessage, ChatCompletionToolChoiceOption,
    CreateChatCompletionRequest, CreateChatCompletionRequestArgs, FinishReason, FunctionCall,
    ToolChoiceOptions,
};
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<ChatCompletionMessageToolCalls>,
}

/// 一次请求里该怎样约束模型对工具的使用。
///
/// `tool_choice` 是**请求级**参数，不能写进 messages，所以策略只能逐轮传入。
/// 没有 `None` 变体：`tools = None` 本身就等价于服务端默认的 `none`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPolicy {
    /// 模型可自由选择：回文本或调工具。
    Auto,
    /// 必须至少调用一个工具。
    Required,
    /// 强制调用指定工具。
    Force(String),
}

impl ToolPolicy {
    fn to_choice(&self) -> ChatCompletionToolChoiceOption {
        match self {
            ToolPolicy::Auto => ChatCompletionToolChoiceOption::Mode(ToolChoiceOptions::Auto),
            ToolPolicy::Required => {
                ChatCompletionToolChoiceOption::Mode(ToolChoiceOptions::Required)
            }
            ToolPolicy::Force(name) => {
                ChatCompletionToolChoiceOption::Function(name.as_str().into())
            }
        }
    }

    /// 端点在 400 里明确拒绝 `tool_choice` 之后的退路。
    fn demote(&self) -> Self {
        ToolPolicy::Auto
    }
}

/// 只有非 `Auto` 的策略才值得为 `tool_choice` 被拒而重发：`Auto` 已经是退路本身。
fn is_forced(policy: &ToolPolicy) -> bool {
    !matches!(policy, ToolPolicy::Auto)
}

/// 组装一次 Chat Completions 请求。
///
/// 抽成纯函数是为了能在不构造 `LLMClient`（因此不需要 provider 配置）的前提下，
/// 断言序列化后的请求体里到底放了什么。
fn build_chat_request(
    model: &str,
    max_tokens: u32,
    messages: &[ChatCompletionRequestMessage],
    tools: Option<&ToolHashMap>,
    policy: &ToolPolicy,
) -> anyhow::Result<CreateChatCompletionRequest> {
    let mut builder = CreateChatCompletionRequestArgs::default();
    builder
        .model(model)
        .messages(messages.to_vec())
        .max_tokens(max_tokens);

    if let Some(tools) = tools {
        builder.tools(tool_definitions(tools)?);
        // 只有声明了工具时才谈得上 tool_choice：`required` 配零工具会被服务端 400 拒绝。
        builder.tool_choice(policy.to_choice());
        // 开启并行：这是「请求」而不是「契约」——端点可以忽略它（DeepSeek 的
        // /chat/completions 参数表里没有这个字段），所以一轮多个 tool_call 必须
        // 当作正常输入处理：ReactLoop 为每个 call 补配对消息，见到 final_answer
        // 立即交付并跳过同轮其余调用。
        builder.parallel_tool_calls(true);
    }

    Ok(builder.build()?)
}

/// 判断错误是否是「端点不接受 `tool_choice`」。
///
/// 只看 400 且明确指向 `tool_choice` 的错误：限流（429）、鉴权（401）等一律不匹配，
/// 免得把真实故障当成降级信号。
fn rejected_tool_choice(err: &OpenAIError) -> bool {
    match err {
        OpenAIError::ApiError(response)
            if response.status_code == reqwest::StatusCode::BAD_REQUEST =>
        {
            response.api_error.param.as_deref() == Some("tool_choice")
                || response.api_error.message.contains("tool_choice")
        }
        _ => false,
    }
}

/// 传输层：编排层唯一依赖的「完成器」。
///
/// 只有一个具体类型，没有 trait：真实后端与测试 / 离线示例用的脚本化后端都收在
/// `backend` 字段里，公开行为由 [`LLMClient::complete`] / [`LLMClient::stream`]
/// 统一派发。请求前后的回调链也在这一层挂载（见 [`LLMClient::with_callbacks`]
/// 与 [`callback`] 模块）：不注册回调时，全链路与没有这层逐字节一致。
pub struct LLMClient {
    backend: Backend,
    callbacks: Vec<Arc<dyn Callback>>,
}

enum Backend {
    Live(Live),
    Scripted(Scripted),
}

impl LLMClient {
    /// 从环境变量读模型与 provider 配置构造真实客户端；配置缺失时 panic。
    pub fn new() -> Self {
        let model = provider::model_id().expect("缺少 CURRENT_USE_MODEL_ID，请检查 .env");
        Self::from_model(&model).expect("缺少 provider 配置，请检查 .env")
    }

    /// 用显式模型 ID 构造：调用方已经读过 `model_id()` 时不必再读一次环境变量，
    /// 也避免 `new()` 在配置缺失时 panic。
    pub fn from_model(model: &str) -> anyhow::Result<Self> {
        Ok(Self {
            backend: Backend::Live(Live::new(model)?),
            callbacks: Vec::new(),
        })
    }

    /// 脚本化：按预置队列返回回复、不触碰网络。测试与离线示例使用。
    pub fn scripted(replies: Vec<Reply>) -> Self {
        Self {
            backend: Backend::Scripted(Scripted::new(replies)),
            callbacks: Vec::new(),
        }
    }

    /// 注册请求前后的回调链，顺序 = 注册顺序：`BeforeSend` 正序、`AfterSend` 逆序。
    pub fn with_callbacks(mut self, callbacks: Vec<Arc<dyn Callback>>) -> Self {
        self.callbacks = callbacks;
        self
    }

    /// 脚本化模式下每次请求的快照（消息 / 工具名 / 策略）；live 模式恒为空。
    /// 仅测试与示例用来断言「实际发出去的是什么」。
    pub fn scripted_requests(&self) -> Vec<ScriptedRequest> {
        match &self.backend {
            Backend::Scripted(scripted) => scripted.requests(),
            Backend::Live(_) => Vec::new(),
        }
    }

    /// 一次性完成：请求先过回调链，拿到回复后再逆序过回调链。
    pub async fn complete(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&ToolHashMap>,
        policy: ToolPolicy,
    ) -> anyhow::Result<Reply> {
        let messages = callback::prepare(&self.callbacks, messages).await?;
        let mut reply = match &self.backend {
            Backend::Live(live) => live.complete(&messages, tools, policy).await?,
            Backend::Scripted(scripted) => scripted.next(&messages, tools, &policy, &mut |_| {})?,
        };
        callback::conclude(&self.callbacks, &messages, &mut reply).await?;
        Ok(reply)
    }

    /// 流式完成：`on_token` 直通内层，token 级改写不在回调接缝范围内。
    pub async fn stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&ToolHashMap>,
        policy: ToolPolicy,
        on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> anyhow::Result<Reply> {
        let messages = callback::prepare(&self.callbacks, messages).await?;
        let mut reply = match &self.backend {
            Backend::Live(live) => live.stream(&messages, tools, policy, on_token).await?,
            Backend::Scripted(scripted) => scripted.next(&messages, tools, &policy, on_token)?,
        };
        callback::conclude(&self.callbacks, &messages, &mut reply).await?;
        Ok(reply)
    }
}

impl Default for LLMClient {
    fn default() -> Self {
        Self::new()
    }
}

/// 真实后端：持有 model 名与 `async_openai` 客户端。
struct Live {
    model: String,
    max_tokens: u32,
    internal_client: async_openai::Client<OpenAIConfig>,
    /// 端点明确拒绝过 `tool_choice` 之后置位：此后所有请求都不再强制，
    /// 避免每一轮都白挨一次 400。粘性标记，进程内只付一次探测成本。
    tool_choice_unsupported: AtomicBool,
}

impl Live {
    fn new(model: &str) -> anyhow::Result<Self> {
        let internal_client = async_openai::Client::with_config(provider::client_config()?);

        Ok(Self {
            model: model.to_owned(),
            max_tokens: provider::max_tokens(),
            internal_client,
            tool_choice_unsupported: AtomicBool::new(false),
        })
    }

    fn request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&ToolHashMap>,
        policy: &ToolPolicy,
    ) -> anyhow::Result<CreateChatCompletionRequest> {
        let effective = self.effective_policy(policy);
        build_chat_request(&self.model, self.max_tokens, messages, tools, &effective)
    }

    /// 端点已经被证实不支持强制时，把所有策略压回 `Auto`。
    fn effective_policy(&self, policy: &ToolPolicy) -> ToolPolicy {
        if self.tool_choice_unsupported.load(Ordering::Relaxed) {
            policy.demote()
        } else {
            policy.clone()
        }
    }

    /// 还值得为 `tool_choice` 被拒重发一次吗：强制策略 + 尚未确认过不支持。
    fn can_fallback(&self, policy: &ToolPolicy) -> bool {
        is_forced(policy) && !self.tool_choice_unsupported.load(Ordering::Relaxed)
    }

    /// 记下「端点不支持 tool_choice」，并返回降级后的策略。
    fn demote_after_rejection(&self, err: &OpenAIError) -> ToolPolicy {
        self.tool_choice_unsupported.store(true, Ordering::Relaxed);
        tracing::warn!("端点拒绝 tool_choice（{err}），后续请求降级为 auto");
        ToolPolicy::Auto
    }

    async fn complete(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&ToolHashMap>,
        policy: ToolPolicy,
    ) -> anyhow::Result<Reply> {
        let request = self.request(messages, tools, &policy)?;
        let response = match self.internal_client.chat().create(request).await {
            Ok(response) => response,
            Err(err) if self.can_fallback(&policy) && rejected_tool_choice(&err) => {
                // 请求是 Clone 的，这里重新按降级策略构造一份重发一次。
                let retry = build_chat_request(
                    &self.model,
                    self.max_tokens,
                    messages,
                    tools,
                    &self.demote_after_rejection(&err),
                )?;
                self.internal_client.chat().create(retry).await?
            }
            Err(err) => return Err(err.into()),
        };
        tracing::info!("LLM Response: {:#?}", response);

        let message = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No choices in response"))?
            .message;

        Ok(Reply {
            content: message.content.unwrap_or_default(),
            tool_calls: message.tool_calls.unwrap_or_default(),
        })
    }

    async fn stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: Option<&ToolHashMap>,
        policy: ToolPolicy,
        on_token: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> anyhow::Result<Reply> {
        let request = self.request(messages, tools, &policy)?;

        let mut content = String::new();
        let mut accumulator = ToolCallAccumulator::default();
        let mut finish_reason = None;

        // `create_stream` 在真正开始收流前就会因 400 失败，此时还没有任何 token 落地，
        // 重发不会造成重复输出。
        let mut stream = match self.internal_client.chat().create_stream(request).await {
            Ok(stream) => stream,
            Err(err) if self.can_fallback(&policy) && rejected_tool_choice(&err) => {
                let retry = build_chat_request(
                    &self.model,
                    self.max_tokens,
                    messages,
                    tools,
                    &self.demote_after_rejection(&err),
                )?;
                self.internal_client.chat().create_stream(retry).await?
            }
            Err(err) => return Err(err.into()),
        };
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;

            let Some(choice) = chunk.choices.first() else {
                continue;
            };

            if let Some(reason) = &choice.finish_reason {
                finish_reason = Some(*reason);
            }

            if let Some(delta) = &choice.delta.content {
                content.push_str(delta);
                on_token(delta);
            }

            for fragment in choice.delta.tool_calls.iter().flatten() {
                accumulator.absorb(fragment);
            }
        }

        let tool_calls = accumulator.finish();

        if content.is_empty()
            && tool_calls.is_empty()
            && finish_reason == Some(FinishReason::Length)
        {
            tracing::warn!(
                "预算（{} token）耗尽却零产出：推理模型的 reasoning 分片会被 async-openai 丢弃，实际消耗不可见。可调大 {MAX_TOKENS_ENV}",
                self.max_tokens
            );
        }

        Ok(Reply {
            content,
            tool_calls,
        })
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    slots: Vec<ToolCallFragment>,
}

impl ToolCallAccumulator {
    fn absorb(&mut self, chunk: &ChatCompletionMessageToolCallChunk) {
        let index = chunk.index as usize;
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, ToolCallFragment::default);
        }
        self.slots[index].absorb(chunk);
    }

    fn finish(self) -> Vec<ChatCompletionMessageToolCalls> {
        self.slots
            .into_iter()
            .filter(|fragment| !fragment.name.is_empty())
            .map(|fragment| {
                ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                    id: fragment.id,
                    function: FunctionCall {
                        name: fragment.name,
                        arguments: fragment.arguments,
                    },
                })
            })
            .collect()
    }
}

#[derive(Default)]
struct ToolCallFragment {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallFragment {
    fn absorb(&mut self, fragment: &async_openai::types::chat::ChatCompletionMessageToolCallChunk) {
        if let Some(id) = &fragment.id {
            self.id.push_str(id);
        }
        if let Some(function) = &fragment.function {
            if let Some(name) = &function.name {
                self.name.push_str(name);
            }
            if let Some(arguments) = &function.arguments {
                self.arguments.push_str(arguments);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::chat::{ChatCompletionMessageToolCallChunk, FunctionCallStream};

    fn fragment(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> ChatCompletionMessageToolCallChunk {
        ChatCompletionMessageToolCallChunk {
            index,
            id: id.map(str::to_owned),
            r#type: None,
            function: Some(FunctionCallStream {
                name: name.map(str::to_owned),
                arguments: arguments.map(str::to_owned),
            }),
        }
    }

    fn names_and_arguments(calls: &[ChatCompletionMessageToolCalls]) -> Vec<(String, String)> {
        calls
            .iter()
            .map(|call| match call {
                ChatCompletionMessageToolCalls::Function(f) => {
                    (f.function.name.clone(), f.function.arguments.clone())
                }
                other => panic!("只应有 function 调用，实际是 {other:?}"),
            })
            .collect()
    }

    #[test]
    fn reassembles_one_call_split_across_chunks() {
        let mut accumulator = ToolCallAccumulator::default();

        accumulator.absorb(&fragment(0, Some("call_1"), Some("echo"), None));
        accumulator.absorb(&fragment(0, None, None, Some(r#"{"q""#)));
        accumulator.absorb(&fragment(0, None, None, Some(r#":"#)));
        accumulator.absorb(&fragment(0, None, None, Some(r#""hi"}"#)));

        assert_eq!(
            names_and_arguments(&accumulator.finish()),
            vec![("echo".to_owned(), r#"{"q":"hi"}"#.to_owned())]
        );
    }

    #[test]
    fn keeps_interleaved_calls_apart() {
        let mut accumulator = ToolCallAccumulator::default();

        accumulator.absorb(&fragment(0, Some("a"), Some("echo"), Some("{}")));
        accumulator.absorb(&fragment(1, Some("b"), Some("nope"), Some("{}")));
        accumulator.absorb(&fragment(0, None, None, Some("tail")));

        assert_eq!(
            names_and_arguments(&accumulator.finish()),
            vec![
                ("echo".to_owned(), "{}tail".to_owned()),
                ("nope".to_owned(), "{}".to_owned()),
            ]
        );
    }

    #[test]
    fn drops_slots_that_never_got_a_name() {
        let mut accumulator = ToolCallAccumulator::default();

        accumulator.absorb(&fragment(0, Some("a"), Some("echo"), Some("{}")));
        accumulator.absorb(&fragment(2, Some("c"), Some("nope"), Some("{}")));

        assert_eq!(
            names_and_arguments(&accumulator.finish()),
            vec![
                ("echo".to_owned(), "{}".to_owned()),
                ("nope".to_owned(), "{}".to_owned()),
            ]
        );
    }

    // ---- 策略 → 请求体 ----

    struct DummyTool;

    #[async_trait::async_trait]
    impl crate::tools::tool::Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }

        fn description(&self) -> &str {
            "测试工具"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, _args_json: &str) -> anyhow::Result<String> {
            Ok("ok".to_owned())
        }
    }

    fn dummy_tools() -> ToolHashMap {
        let mut tools = ToolHashMap::new();
        tools.insert(
            "dummy".to_owned(),
            std::sync::Arc::new(DummyTool) as std::sync::Arc<dyn crate::tools::tool::Tool>,
        );
        tools
    }

    fn request_json(tools: Option<&ToolHashMap>, policy: &ToolPolicy) -> serde_json::Value {
        let request = build_chat_request("m", 128, &[], tools, policy).expect("构造请求失败");
        serde_json::to_value(&request).expect("序列化请求失败")
    }

    #[test]
    fn required_policy_forces_tool_choice_and_allows_parallel_calls() {
        let tools = dummy_tools();
        let json = request_json(Some(&tools), &ToolPolicy::Required);

        assert_eq!(json["tool_choice"], "required");
        assert_eq!(json["parallel_tool_calls"], true);
    }

    #[test]
    fn force_policy_names_the_function() {
        let tools = dummy_tools();
        let json = request_json(Some(&tools), &ToolPolicy::Force("final_answer".to_owned()));

        assert_eq!(json["tool_choice"]["type"], "function");
        assert_eq!(json["tool_choice"]["function"]["name"], "final_answer");
    }

    #[test]
    fn omits_tool_choice_when_no_tools_are_declared() {
        // `required` 配零工具会被服务端 400 拒绝，所以无工具时必须整个字段消失。
        let json = request_json(None, &ToolPolicy::Required);

        assert!(json.get("tools").is_none());
        assert!(json.get("tool_choice").is_none(), "实际请求体：{json}");
    }

    #[test]
    fn only_forced_policies_are_worth_a_retry() {
        assert!(!is_forced(&ToolPolicy::Auto));
        assert!(is_forced(&ToolPolicy::Required));
        assert!(is_forced(&ToolPolicy::Force("final_answer".to_owned())));
        assert_eq!(ToolPolicy::Required.demote(), ToolPolicy::Auto);
    }

    // ---- tool_choice 被拒的判定 ----

    fn api_error(status: reqwest::StatusCode, message: &str, param: Option<&str>) -> OpenAIError {
        OpenAIError::ApiError(async_openai::error::ApiErrorResponse {
            status_code: status,
            api_error: async_openai::error::ApiError {
                message: message.to_owned(),
                r#type: None,
                param: param.map(str::to_owned),
                code: None,
            },
        })
    }

    #[test]
    fn detects_tool_choice_rejection() {
        assert!(rejected_tool_choice(&api_error(
            reqwest::StatusCode::BAD_REQUEST,
            "unsupported parameter",
            Some("tool_choice"),
        )));
        assert!(rejected_tool_choice(&api_error(
            reqwest::StatusCode::BAD_REQUEST,
            "tool_choice is not supported for this model",
            None,
        )));
    }

    #[test]
    fn ignores_unrelated_api_errors() {
        assert!(!rejected_tool_choice(&api_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "rate limited",
            None,
        )));
        assert!(!rejected_tool_choice(&api_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "tool_choice",
            None,
        )));
        assert!(!rejected_tool_choice(&api_error(
            reqwest::StatusCode::BAD_REQUEST,
            "bad messages",
            Some("messages"),
        )));
    }
}
