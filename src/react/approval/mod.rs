//! 危险工具的确认闸门：循环在策略判定「要问」时通过 [`Confirmer`] 征得决定。
//!
//! 与配置解耦：`Confirmer` 只回答「批准还是拒绝」，「哪些工具要问」由
//! [`crate::settings::ApprovalPolicy`] 决定，两者都由调用方注入 `ReactLoop`。
//!
//! [`Confirmer`] 是**扩展点**（trait）：输入审核请求，输出用户决策；在用户做出决策之前
//! **一直等待**（不超时、不默认、不替用户决定）。向用户询问的方式可以有多种，本 crate
//! 目前只内置终端实现 [`TerminalConfirmer`]；测试与离线示例用 [`ScriptedConfirmer`]；
//! 想让闭包/函数直接充当确认方用 [`FnConfirmer`]。

mod scripted;
mod terminal;

pub use scripted::ScriptedConfirmer;
pub use terminal::TerminalConfirmer;

use std::future::Future;

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
    /// [`TerminalConfirmer`] 只在 EOF（无人可答）时返回它。
    Pending,
}

/// 扩展点：向用户征求一次确认的方式。
///
/// 契约：输入一个审核请求，输出用户决策；在用户做出决策之前**一直等待**
/// （不超时、不默认、不替用户决定）。没有用户可问时返回 [`Decision::Pending`]。
#[async_trait::async_trait]
pub trait Confirmer: Send + Sync {
    async fn confirm(&self, request: &ApprovalRequest) -> Decision;
}

/// 把闭包/函数适配成 [`Confirmer`]，免去为一次性场景定义命名类型。
///
/// 典型用途：测试桩（`|_| async { Decision::Approve }`）、装饰内层确认方
/// （日志 / 审计 / 组合）、下游桥接（GUI 事件循环 → oneshot）。
pub struct FnConfirmer<F> {
    ask: F,
}

impl<F, Fut> FnConfirmer<F>
where
    F: Fn(ApprovalRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Decision> + Send + 'static,
{
    pub fn new(ask: F) -> Self {
        Self { ask }
    }
}

#[async_trait::async_trait]
impl<F, Fut> Confirmer for FnConfirmer<F>
where
    F: Fn(ApprovalRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Decision> + Send + 'static,
{
    async fn confirm(&self, request: &ApprovalRequest) -> Decision {
        (self.ask)(request.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            turn: 1,
            tool: "echo".to_owned(),
            description: String::new(),
            arguments: "{}".to_owned(),
        }
    }

    #[tokio::test]
    async fn fn_confirmer_adapts_closures_into_trait_objects() {
        let confirmer: Arc<dyn Confirmer> =
            Arc::new(FnConfirmer::new(|_| async { Decision::Approve }));
        assert_eq!(confirmer.confirm(&request()).await, Decision::Approve);
    }
}
