//! 危险工具的确认接缝：循环在策略判定「要问」时通过 [`Confirmer`] 征得决定。
//!
//! 与配置解耦：`Confirmer` 只回答「批准还是拒绝」，「哪些工具要问」由
//! [`crate::settings::ApprovalPolicy`] 决定，两者都由调用方注入 `ReactLoop`。

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
}

/// 问谁、怎么问、要不要记住，都由实现决定。循环只等一个 [`Decision`]。
#[async_trait::async_trait]
pub trait Confirmer: Send + Sync {
    async fn confirm(&self, request: &ApprovalRequest) -> Decision;
}
