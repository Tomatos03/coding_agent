//! 消息发送前后的回调接缝。
//!
//! 回调链直接挂在唯一的具体类型 [`LLMClient`](super::models::LLMClient) 上：注册用
//! [`LLMClient::with_callbacks`](super::models::LLMClient::with_callbacks)，派发在
//! [`prepare`] / [`conclude`] 里完成——每轮请求发出前**正序**派发
//! [`CallbackEvent::BeforeSend`]、响应返回后**逆序**派发 [`CallbackEvent::AfterSend`]。
//! 不注册回调时，全链路与没有这层逐字节一致。
//!
//! 两个时机的语义差别是理解这个模块的关键：
//!
//! - `BeforeSend` 改的是「这一次寄出去的信」——每轮都从当前 `History` 重新克隆、重新
//!   派发，改动**不落历史**；这正是「发出去的比存下来的少」的裁剪刚需。
//! - `AfterSend` 改的是「回信」——`Reply` 回到 `ReactLoop` 后会原样落历史并驱动后续
//!   （改掉的 `tool_calls` 会被执行），所以它是**持久**的。
//!
//! 根特征 [`Callback`] 只定义 [`Callback::call`] 一个方法，挂点做成数据（枚举变体）：
//! 将来新增回调点时，trait、装饰器、所有既有实现都不动，只多一个变体；每个实现用
//! 「`let`-`else` 放行」模板只处理自己关心的变体即可。

use std::sync::Arc;

use async_openai::types::chat::ChatCompletionRequestMessage;

use super::models::Reply;

/// 一次挂点时机携带的一切。每个变体只带该时机「合法可改」的东西，能力约束做进类型：
/// 请求还没发出去时改消息有意义；响应已经回来时消息只读、回复可改。
///
/// `#[non_exhaustive]` 让将来新增变体不算破坏性变更——下游实现用
/// `let CallbackEvent::X { .. } = event else { return Ok(()) };` 的模板即可对新增变体免疫。
#[non_exhaustive]
pub enum CallbackEvent<'a> {
    /// 请求发出前。`messages` 是本次请求的完整消息序列（已含 system），增删改都只影响
    /// 这一次请求，不回写 `History`。
    BeforeSend {
        messages: &'a mut Vec<ChatCompletionRequestMessage>,
    },
    /// 响应返回后、交给调用方（`ReactLoop`）之前。改 `Reply` 等价于改「将要落历史 /
    /// 交付的内容」；`messages` 是实际发出去的版本，只读。
    AfterSend {
        messages: &'a [ChatCompletionRequestMessage],
        reply: &'a mut Reply,
    },
}

/// 根特征：所有回调——无论挂在哪个时机——都实现这唯一一个方法。
///
/// 不关心的事件原样放行即可：
///
/// ```ignore
/// async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
///     let CallbackEvent::BeforeSend { messages } = event else {
///         return Ok(()); // 不关心的事件直接放行
///     };
///     // ……
///     Ok(())
/// }
/// ```
///
/// [`call`]: Callback::call
#[async_trait::async_trait]
pub trait Callback: Send + Sync {
    async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()>;
}

/// 克隆 + 正序派发 `BeforeSend`。返回的 `Vec` 就是实际发出去的版本。
pub(crate) async fn prepare(
    callbacks: &[Arc<dyn Callback>],
    messages: &[ChatCompletionRequestMessage],
) -> anyhow::Result<Vec<ChatCompletionRequestMessage>> {
    let mut messages = messages.to_vec();
    for callback in callbacks {
        callback
            .call(CallbackEvent::BeforeSend {
                messages: &mut messages,
            })
            .await?;
    }
    Ok(messages)
}

/// 逆序派发 `AfterSend`（洋葱模型）。
///
/// `messages` 是实际发出去的版本，仅供观察；改回复才是这里的副作用。
pub(crate) async fn conclude(
    callbacks: &[Arc<dyn Callback>],
    messages: &[ChatCompletionRequestMessage],
    reply: &mut Reply,
) -> anyhow::Result<()> {
    for callback in callbacks.iter().rev() {
        callback
            .call(CallbackEvent::AfterSend { messages, reply })
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_openai::types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
    };

    use super::*;
    use crate::agent::llm::models::{LLMClient, ToolPolicy};

    /// 构造一条 user 消息，供测试注入 / 断言。
    fn user(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestUserMessageArgs::default()
            .content(text)
            .build()
            .expect("构造 user 消息失败")
            .into()
    }

    /// 把消息序列化成 JSON 文本做包含判断——避免依赖各类型的 `PartialEq`。
    fn text(message: &ChatCompletionRequestMessage) -> String {
        serde_json::to_string(message).expect("序列化消息失败")
    }

    /// 脚本后端每次实际收到的消息（已过回调链）。
    fn seen(llm: &LLMClient) -> Vec<Vec<ChatCompletionRequestMessage>> {
        llm.scripted_requests()
            .into_iter()
            .map(|request| request.messages)
            .collect()
    }

    /// 在 `BeforeSend` 往尾部追加一条注入消息。
    struct AppendMarker(&'static str);

    #[async_trait::async_trait]
    impl Callback for AppendMarker {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            if let CallbackEvent::BeforeSend { messages } = event {
                messages.push(user(self.0));
            }
            Ok(())
        }
    }

    /// 只在 `BeforeSend` 报错。
    struct FailBefore;

    #[async_trait::async_trait]
    impl Callback for FailBefore {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            if let CallbackEvent::BeforeSend { .. } = event {
                anyhow::bail!("before boom");
            }
            Ok(())
        }
    }

    /// 只在 `AfterSend` 报错。
    struct FailAfter;

    #[async_trait::async_trait]
    impl Callback for FailAfter {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            if let CallbackEvent::AfterSend { .. } = event {
                anyhow::bail!("after boom");
            }
            Ok(())
        }
    }

    /// 只处理 `BeforeSend`、其余事件原样放行；记录收到的事件种类。
    struct PassThrough {
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl Callback for PassThrough {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            let kind = match event {
                CallbackEvent::BeforeSend { .. } => "before",
                CallbackEvent::AfterSend { .. } => "after",
            };
            self.events.lock().expect("锁被毒化").push(kind);
            Ok(())
        }
    }

    /// 洋葱模型的外层：`BeforeSend` 追加标记消息，`AfterSend` 记录最终回复。
    struct Outer {
        trace: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Callback for Outer {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            match event {
                CallbackEvent::BeforeSend { messages } => {
                    self.trace
                        .lock()
                        .expect("锁被毒化")
                        .push("outer:before".into());
                    messages.push(user("from-outer"));
                }
                CallbackEvent::AfterSend { reply, .. } => {
                    self.trace
                        .lock()
                        .expect("锁被毒化")
                        .push(format!("outer:after:{}", reply.content));
                }
            }
            Ok(())
        }
    }

    /// 洋葱模型的内层：`BeforeSend` 记录是否看到外层的改动，`AfterSend` 改写回复。
    struct Inner {
        trace: Arc<Mutex<Vec<String>>>,
        saw_outer: Arc<Mutex<bool>>,
    }

    #[async_trait::async_trait]
    impl Callback for Inner {
        async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
            match event {
                CallbackEvent::BeforeSend { messages } => {
                    *self.saw_outer.lock().expect("锁被毒化") =
                        messages.iter().any(|m| text(m).contains("from-outer"));
                    self.trace
                        .lock()
                        .expect("锁被毒化")
                        .push("inner:before".into());
                }
                CallbackEvent::AfterSend { reply, .. } => {
                    self.trace
                        .lock()
                        .expect("锁被毒化")
                        .push("inner:after".into());
                    reply.content = "rewritten-by-inner".into();
                }
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn before_send_injection_reaches_inner() {
        let llm = LLMClient::scripted(vec![Reply::default()])
            .with_callbacks(vec![Arc::new(AppendMarker("INJECTED"))]);
        let base = vec![user("hi")];

        llm.complete(&base, None, ToolPolicy::Auto)
            .await
            .expect("complete 应成功");

        let seen = seen(&llm);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].len(), 2, "注入后后端应收到 2 条消息");
        assert!(
            text(&seen[0][1]).contains("INJECTED"),
            "注入的消息应送达后端且位于尾部"
        );
    }

    #[tokio::test]
    async fn after_send_rewrites_reply() {
        struct Rewrite;
        #[async_trait::async_trait]
        impl Callback for Rewrite {
            async fn call(&self, event: CallbackEvent<'_>) -> anyhow::Result<()> {
                if let CallbackEvent::AfterSend { reply, .. } = event {
                    reply.content = "clean".into();
                }
                Ok(())
            }
        }

        let llm = LLMClient::scripted(vec![Reply {
            content: "raw".into(),
            tool_calls: Vec::new(),
        }])
        .with_callbacks(vec![Arc::new(Rewrite)]);
        let reply = llm
            .complete(&[user("hi")], None, ToolPolicy::Auto)
            .await
            .expect("complete 应成功");

        assert_eq!(reply.content, "clean", "返回给调用方的应是改写后的回复");
    }

    #[tokio::test]
    async fn empty_callbacks_is_passthrough() {
        let llm = LLMClient::scripted(vec![Reply {
            content: "same".into(),
            tool_calls: Vec::new(),
        }]);

        let reply = llm
            .complete(&[user("hi")], None, ToolPolicy::Auto)
            .await
            .expect("complete 应成功");

        assert_eq!(reply.content, "same");
    }

    #[tokio::test]
    async fn onion_order_and_cross_visibility() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let saw_outer = Arc::new(Mutex::new(false));
        let llm = LLMClient::scripted(vec![Reply {
            content: "raw".into(),
            tool_calls: Vec::new(),
        }])
        .with_callbacks(vec![
            Arc::new(Outer {
                trace: trace.clone(),
            }),
            Arc::new(Inner {
                trace: trace.clone(),
                saw_outer: saw_outer.clone(),
            }),
        ]);

        let reply = llm
            .complete(&[user("hi")], None, ToolPolicy::Auto)
            .await
            .expect("complete 应成功");

        assert_eq!(
            *trace.lock().expect("锁被毒化"),
            vec![
                "outer:before".to_owned(),
                "inner:before".to_owned(),
                "inner:after".to_owned(),
                "outer:after:rewritten-by-inner".to_owned(),
            ],
            "BeforeSend 正序、AfterSend 逆序"
        );
        assert!(*saw_outer.lock().expect("锁被毒化"), "内层应看到外层的注入");
        assert_eq!(reply.content, "rewritten-by-inner", "外层应看到内层的改写");
    }

    #[tokio::test]
    async fn before_send_error_aborts_without_calling_inner() {
        let llm =
            LLMClient::scripted(vec![Reply::default()]).with_callbacks(vec![Arc::new(FailBefore)]);

        let result = llm.complete(&[user("hi")], None, ToolPolicy::Auto).await;

        assert!(result.is_err(), "BeforeSend 报错应中止整个请求");
        assert!(
            llm.scripted_requests().is_empty(),
            "后端不应被调用（fail-closed）"
        );
    }

    #[tokio::test]
    async fn after_send_error_propagates() {
        let llm = LLMClient::scripted(vec![Reply {
            content: "raw".into(),
            tool_calls: Vec::new(),
        }])
        .with_callbacks(vec![Arc::new(FailAfter)]);

        let result = llm.complete(&[user("hi")], None, ToolPolicy::Auto).await;

        assert!(result.is_err(), "即便回复已到手，AfterSend 报错也要传播");
    }

    #[tokio::test]
    async fn pass_through_template_participates_in_full_chain() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let llm = LLMClient::scripted(vec![Reply {
            content: "untouched".into(),
            tool_calls: Vec::new(),
        }])
        .with_callbacks(vec![Arc::new(PassThrough {
            events: events.clone(),
        })]);

        let reply = llm
            .complete(&[user("hi")], None, ToolPolicy::Auto)
            .await
            .expect("complete 应成功");

        assert_eq!(
            *events.lock().expect("锁被毒化"),
            vec!["before", "after"],
            "只处理 BeforeSend 的实现也应收到两个事件"
        );
        assert_eq!(reply.content, "untouched", "放行模板不改回复");
    }

    #[tokio::test]
    async fn stream_dispatches_both_events_and_leaves_tokens() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let llm = LLMClient::scripted(vec![Reply {
            content: "hello".into(),
            tool_calls: Vec::new(),
        }])
        .with_callbacks(vec![Arc::new(PassThrough {
            events: events.clone(),
        })]);

        let mut tokens: Vec<String> = Vec::new();
        let reply = llm
            .stream(&[user("hi")], None, ToolPolicy::Auto, &mut |token| {
                tokens.push(token.to_owned());
            })
            .await
            .expect("stream 应成功");

        assert_eq!(
            *events.lock().expect("锁被毒化"),
            vec!["before", "after"],
            "stream 路径同样派发两种事件"
        );
        assert_eq!(tokens, vec!["hello".to_owned()], "on_token 直通、不受影响");
        assert!(seen(&llm).len() == 1, "后端应收到一次请求");
        assert_eq!(reply.content, "hello");
    }
}
