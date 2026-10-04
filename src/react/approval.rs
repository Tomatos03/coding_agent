//! 危险工具的确认闸门：循环在策略判定「要问」时通过 [`Confirmer`] 征得决定。
//!
//! 与配置解耦：`Confirmer` 只回答「批准还是拒绝」，「哪些工具要问」由
//! [`crate::settings::ApprovalPolicy`] 决定，两者都由调用方注入 `ReactLoop`。
//!
//! 与传输层的 `LLMClient` 同构：只有一个具体类型、没有 trait，两种来源收在内部——
//! [`Confirmer::new`] 由调用方提供「怎么问、怎么答」的异步应答器（终端、GUI、批处理……），
//! [`Confirmer::scripted`] 按预置队列作答，供测试与离线示例使用。

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

use futures::future::BoxFuture;

/// 一次待确认的工具调用。字段是给人看的；参数级粒度（如 `rm` 拦、`ls` 放行）
/// 由确认方拿到原始 `arguments` 后自行判断。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub turn: usize,
    pub tool: String,
    pub description: String,
    pub arguments: String,
}

/// 确认结果。拒绝**不会**中断循环：它以 Observation 的形式回给模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    /// 「我现在不决定」：循环**挂起**本次 run（`Termination::Suspended`），
    /// 历史停在未配对的 `tool_call` 上，等调用方带决定 `resume`。
    Pending,
}

/// 异步应答器：拿到请求（按值）、给出决定。
type Ask = dyn Fn(ApprovalRequest) -> BoxFuture<'static, Decision> + Send + Sync;

/// 脚本化状态：预置决策队列 + 每次询问的快照。
struct Scripted {
    decisions: VecDeque<Decision>,
    requests: Vec<ApprovalRequest>,
}

enum Backend {
    /// 怎么问、要不要记住，由调用方决定。
    Ask(Box<Ask>),
    /// 按预置队列依次作答。
    Scripted(Mutex<Scripted>),
}

/// 工具执行前的确认方：策略判 `ask` 时，循环交出 [`ApprovalRequest`] 换一个 [`Decision`]；
/// 没有注入时该调用会被**挂起**（`Termination::Suspended`），而不是旧版的直接拒绝。
pub struct Confirmer {
    backend: Backend,
}

impl Confirmer {
    /// 交互式：问与答由调用方提供（读终端、弹 GUI、批处理应答……）。
    pub fn new<F, Fut>(ask: F) -> Self
    where
        F: Fn(ApprovalRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Decision> + Send + 'static,
    {
        Self {
            backend: Backend::Ask(Box::new(move |request| Box::pin(ask(request)))),
        }
    }

    /// 脚本化：按预置队列依次给出决策、不触碰任何外部输入。测试与离线示例使用。
    ///
    /// 队列与询问次数应严格对应，耗尽后 panic；每次询问的快照可用
    /// [`Confirmer::scripted_requests`] 取回。
    pub fn scripted(decisions: Vec<Decision>) -> Self {
        Self {
            backend: Backend::Scripted(Mutex::new(Scripted {
                decisions: decisions.into(),
                requests: Vec::new(),
            })),
        }
    }

    /// 脚本化模式下每次询问的快照；交互模式恒为空。
    /// 仅测试与示例用来断言「实际问了什么」。
    pub fn scripted_requests(&self) -> Vec<ApprovalRequest> {
        match &self.backend {
            Backend::Scripted(scripted) => {
                let scripted = scripted.lock().expect("脚本锁被毒化");
                scripted.requests.clone()
            }
            Backend::Ask(_) => Vec::new(),
        }
    }

    /// 征得一个决定；只应由 `ReactLoop` 调用。
    pub(crate) async fn confirm(&self, request: &ApprovalRequest) -> Decision {
        match &self.backend {
            Backend::Ask(ask) => ask(request.clone()).await,
            Backend::Scripted(scripted) => {
                let mut scripted = scripted.lock().expect("脚本锁被毒化");
                scripted.requests.push(request.clone());
                let decision = scripted.decisions.pop_front().expect("预置确认决策已用尽");
                tracing::info!(tool = %request.tool, arguments = %request.arguments, ?decision, "脚本化确认");
                decision
            }
        }
    }
}
