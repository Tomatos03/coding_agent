//! 脚本化实现：预置决策队列 + 请求快照，供测试与离线示例替掉真实人工输入。

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{ApprovalRequest, Confirmer, Decision};

/// 脚本化确认方：按预置队列依次给出决策、不触碰任何外部输入。
///
/// 队列与询问次数应严格对应，耗尽后 panic；每次询问的快照可用
/// [`ScriptedConfirmer::requests`] 取回。
pub struct ScriptedConfirmer {
    state: Mutex<Scripted>,
}

struct Scripted {
    decisions: VecDeque<Decision>,
    requests: Vec<ApprovalRequest>,
}

impl ScriptedConfirmer {
    pub fn new(decisions: Vec<Decision>) -> Self {
        Self {
            state: Mutex::new(Scripted {
                decisions: decisions.into(),
                requests: Vec::new(),
            }),
        }
    }

    /// 每次询问的快照。仅测试与示例用来断言「实际问了什么」。
    pub fn requests(&self) -> Vec<ApprovalRequest> {
        self.state.lock().expect("脚本锁被毒化").requests.clone()
    }
}

#[async_trait::async_trait]
impl Confirmer for ScriptedConfirmer {
    async fn confirm(&self, request: &ApprovalRequest) -> Decision {
        let mut state = self.state.lock().expect("脚本锁被毒化");
        state.requests.push(request.clone());
        let decision = state.decisions.pop_front().expect("预置确认决策已用尽");
        tracing::info!(tool = %request.tool, arguments = %request.arguments, ?decision, "脚本化确认");
        decision
    }
}
