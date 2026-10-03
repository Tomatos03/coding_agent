pub const DEFAULT_MAX_TURNS: usize = 12;

/// 一次「停在半途」的审批：`tool_call_id` 用于在恢复时定位到批次里的那一个调用。
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub tool_call_id: String,
    pub request: crate::react::approval::ApprovalRequest,
}

#[derive(Debug, Clone)]
pub enum Step {
    Thought {
        turn: usize,
        content: String,
    },
    Answer {
        turn: usize,
        content: String,
    },
    Action {
        turn: usize,
        name: String,
        arguments: String,
    },
    Observation {
        turn: usize,
        name: String,
        output: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// 模型直接以纯文本收尾（端点无视 `required` 时的降级路径，或未开工具的单轮对话）。
    ModelFinished,
    /// 模型调用了 `final_answer`：这是 `required` 下唯一的正常终止方式。
    FinalAnswer,
    /// 撞到轮次上限：答案优先来自收尾轮强制调用的 `final_answer`；端点未配合或
    /// 参数非法时退回 content，仍为空则交付兜底文案，保证 run 不因收尾失败而报错。
    MaxTurns,
    /// 模型既没有 tool_calls 也没有内容。
    EmptyReply,
    /// 审批挂起：本次 run 正常结束，但会话停在未执行完的工具批次上，
    /// 待审内容见 `Outcome.pending`，由调用方 `ReactLoop::resume` 带决定继续。
    Suspended,
}

#[derive(Debug)]
pub struct Outcome {
    pub answer: String,
    pub turns: usize,
    pub termination: Termination,
    /// 仅 `Termination::Suspended` 时非 `None`：等待决定的那个工具调用。
    pub pending: Option<PendingApproval>,
}
