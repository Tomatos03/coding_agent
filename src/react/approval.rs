//! 危险工具的确认闸门：循环在策略判定「要问」时通过 [`Confirmer`] 征得决定。
//!
//! 与配置解耦：`Confirmer` 只回答「批准还是拒绝」，「哪些工具要问」由
//! [`crate::settings::ApprovalPolicy`] 决定，两者都由调用方注入 `ReactLoop`。
//!
//! 与传输层的 `LLMClient` 同构：只有一个具体类型、没有 trait，三种来源收在内部——
//! [`Confirmer::new`] 由调用方提供「怎么问、怎么答」的异步应答器（GUI、批处理……），
//! [`Confirmer::interactive`] 提供终端 y/n 交互：未做出选择则一直等待，EOF（无人可答）
//! 与「没有注入 confirmer」同路——挂起；[`Confirmer::scripted`] 按预置队列作答，
//! 供测试与离线示例使用。

use std::collections::VecDeque;
use std::future::Future;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

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
    /// [`Confirmer::interactive`] 只在 EOF（无人可答）时返回它。
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
    /// 自定义：问与答由调用方提供（弹 GUI、批处理应答……）。
    pub fn new<F, Fut>(ask: F) -> Self
    where
        F: Fn(ApprovalRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Decision> + Send + 'static,
    {
        Self {
            backend: Backend::Ask(Box::new(move |request| Box::pin(ask(request)))),
        }
    }

    /// 交互式：打印请求详情，循环读行直到拿到明确的 y / n——未做出选择就一直等待，
    /// 不超时、不默认、不替用户决定。EOF（无人可答）返回 [`Decision::Pending`] 挂起，
    /// 与「没有注入 confirmer」同一条路径；交互只可能以 Approve / Deny / EOF 收场。
    ///
    /// `next_line` 是行来源：共享终端句柄、独立 stdin、GUI 输入框均可；`None` 表示输入流结束。
    pub fn interactive<F, Fut>(next_line: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<String>> + Send + 'static,
    {
        let next_line = Arc::new(next_line);
        Self::new(move |request| {
            let next_line = Arc::clone(&next_line);
            async move {
                print_request(&request);
                loop {
                    print!("       选择 [y] 批准 / [n] 拒绝：");
                    let _ = std::io::stdout().flush();
                    match next_line().await.as_deref().map(str::trim) {
                        Some("y" | "Y") => return Decision::Approve,
                        Some("n" | "N") => return Decision::Deny,
                        Some(_) => println!("       请输入 y 或 n"),
                        None => return Decision::Pending,
                    }
                }
            }
        })
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

fn print_request(request: &ApprovalRequest) {
    println!();
    println!("[审批] 工具 `{}` 请求执行", request.tool);
    if !request.description.is_empty() {
        println!("       说明：{}", request.description);
    }
    println!("       参数：{}", request.arguments);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存行源：按序吐出预置行；`None` 或耗尽都表示 EOF。
    fn lines(
        lines: &[Option<&str>],
    ) -> impl Fn() -> std::future::Ready<Option<String>> + Send + Sync + 'static {
        let queue: VecDeque<Option<String>> =
            lines.iter().map(|line| line.map(str::to_owned)).collect();
        let queue = Arc::new(Mutex::new(queue));
        move || std::future::ready(queue.lock().expect("行源锁被毒化").pop_front().flatten())
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            turn: 1,
            tool: "echo".to_owned(),
            description: String::new(),
            arguments: "{}".to_owned(),
        }
    }

    #[tokio::test]
    async fn interactive_maps_y_and_n() {
        let confirmer = Confirmer::interactive(lines(&[Some("y")]));
        assert_eq!(confirmer.confirm(&request()).await, Decision::Approve);

        let confirmer = Confirmer::interactive(lines(&[Some(" N ")]));
        assert_eq!(confirmer.confirm(&request()).await, Decision::Deny);
    }

    #[tokio::test]
    async fn interactive_keeps_waiting_on_invalid_input() {
        let confirmer = Confirmer::interactive(lines(&[Some(""), Some("x"), Some("Y")]));
        assert_eq!(confirmer.confirm(&request()).await, Decision::Approve);
    }

    #[tokio::test]
    async fn interactive_eof_suspends() {
        let confirmer = Confirmer::interactive(lines(&[None]));
        assert_eq!(confirmer.confirm(&request()).await, Decision::Pending);
    }
}
