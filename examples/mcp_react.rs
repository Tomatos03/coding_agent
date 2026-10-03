//! 端到端示例：用户提问 → ReAct 循环 → 调用 MCP 工具（先被拒绝、后获批准）→ 汇总回答。
//!
//! 用脚本化的 `LLMClient` + `Confirmer` 代替真实 LLM 与人工确认，**无需任何凭证、可离线运行**：
//! - 第 1 轮：模型请求调用 MCP 工具 `{server}__echo`，审批策略判 ask，脚本确认方**拒绝**；
//!   拒绝被压成 Observation，循环不中断；
//! - 第 2 轮：模型换参数重试，脚本确认方**批准**，工具真实执行；
//! - 第 3 轮：模型根据工具观察调用 `final_answer` 收尾（循环内每轮 `tool_choice=required`）。
//!
//! 运行（需要本机 python3）：
//!   cargo run --example mcp_react
//!   cargo run --example mcp_react -- npx -y @modelcontextprotocol/server-everything

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, FunctionCall,
};
use coding_agent::bootstrap::init;
use coding_agent::constant::prompt::SYSTEM_PROMPT;
use coding_agent::llm::models::{LLMClient, Reply};
use coding_agent::react::approval::{ApprovalRequest, Confirmer, Decision};
use coding_agent::react::models::{DEFAULT_MAX_TURNS, Step};
use coding_agent::react::runner::ReactLoop;
use coding_agent::settings::{ApprovalAction, ApprovalPolicy, ApprovalRule};
use coding_agent::tools::build_tools_with;
use coding_agent::tools::local::final_answer::FINAL_ANSWER_TOOL;
use coding_agent::tools::mcp::{McpConfig, McpServerConfig};

/// 脚本化确认方：按预置队列依次给出决策，并打印每次询问（替代人工确认）。
struct ScriptedConfirmer {
    decisions: Mutex<VecDeque<Decision>>,
}

impl ScriptedConfirmer {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into()),
        }
    }
}

#[async_trait::async_trait]
impl Confirmer for ScriptedConfirmer {
    async fn confirm(&self, request: &ApprovalRequest) -> Decision {
        let decision = self
            .decisions
            .lock()
            .expect("脚本锁被毒化")
            .pop_front()
            .expect("预置确认决策已用尽");
        let label = match decision {
            Decision::Approve => "批准",
            Decision::Deny => "拒绝",
            // 脚本确认方不会返回它；挂起路径的演示见 session 机制的示例。
            Decision::Pending => "稍后决定（挂起）",
        };
        println!(
            "[确认] 工具 `{}` 参数 {}（脚本决策：{label}）",
            request.tool, request.arguments
        );
        decision
    }
}

/// 构造一条「思考 + 调用某个工具」的回复。
fn mcp_tool_call(id: &str, tool_name: &str, arguments: &str) -> Reply {
    Reply {
        content: "我先调用 MCP 的 echo 工具确认链路是否可用。".to_owned(),
        tool_calls: vec![ChatCompletionMessageToolCalls::Function(
            ChatCompletionMessageToolCall {
                id: id.to_owned(),
                function: FunctionCall {
                    name: tool_name.to_owned(),
                    arguments: arguments.to_owned(),
                },
            },
        )],
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    // 默认用测试 fixture；也可通过参数换成任意 stdio MCP server。
    let mut command_args: Vec<String> = std::env::args().skip(1).collect();
    if command_args.is_empty() {
        command_args = vec![
            "python3".to_owned(),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/fake_mcp_server.py"
            )
            .to_owned(),
        ];
    }
    let command = command_args.remove(0);

    let config = McpConfig {
        servers: [(
            "probe".to_owned(),
            McpServerConfig {
                command,
                args: command_args,
                env: Default::default(),
                cwd: None,
                required: false,
                timeout_secs: 60,
            },
        )]
        .into_iter()
        .collect(),
    };

    // 构建「本地 + MCP」工具表（会启动 MCP 子进程并完成握手）。
    let tools = build_tools_with(config).await?;

    // 找到 MCP echo 工具的真实暴露名，再据此编排脚本回复。
    let echo_tool = tools
        .keys()
        .find(|name| name.ends_with("__echo"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("工具表里没有 MCP echo 工具，请检查 server 是否连接成功"))?;

    let mut names: Vec<&str> = tools.keys().map(String::as_str).collect();
    names.sort_unstable();
    println!("已注册工具：{}\n", names.join(", "));

    let user_question = "请调用 MCP 工具帮我确认链路是否可用。";
    let arguments = serde_json::json!({ "text": "hello from agent" }).to_string();
    let retry_arguments = serde_json::json!({ "text": "hello again, after denial" }).to_string();

    let llm = LLMClient::scripted(vec![
        // 第 1 轮：模型决定调用 MCP 工具，脚本确认方拒绝——拒绝被压成 Observation。
        mcp_tool_call("call_mcp_1", &echo_tool, &arguments),
        // 第 2 轮：模型换参数重试，脚本确认方批准，工具真实执行。
        mcp_tool_call("call_mcp_2", &echo_tool, &retry_arguments),
        // 第 3 轮：模型根据 Observation 调用 final_answer 收尾（required 下唯一的终止方式）。
        Reply {
            content: String::new(),
            tool_calls: vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: "call_final_1".to_owned(),
                    function: FunctionCall {
                        name: FINAL_ANSWER_TOOL.to_owned(),
                        arguments: serde_json::json!({
                            "answer": "MCP 链路已打通：第一次调用被拒绝，第二次获批准并成功返回。"
                        })
                        .to_string(),
                    },
                },
            )],
        },
    ]);

    // 审批策略：只对 MCP echo 工具判 ask（其余放行）；确认方脚本预置「先拒绝、后批准」。
    let policy = ApprovalPolicy {
        rules: vec![ApprovalRule {
            pattern: echo_tool.clone(),
            action: ApprovalAction::Ask,
        }],
        ..Default::default()
    };
    let confirmer = ScriptedConfirmer::new(vec![Decision::Deny, Decision::Approve]);

    let mut agent = ReactLoop::new(Arc::new(llm), tools, SYSTEM_PROMPT, DEFAULT_MAX_TURNS)?
        .with_approval_policy(policy)
        .with_confirmer(Arc::new(confirmer));

    println!("审批策略：`{echo_tool}` 需要人工确认（脚本决策：先拒绝、后批准）");
    println!("User: {user_question}\n");

    let outcome = agent
        .run(
            user_question,
            &mut |step| match step {
                Step::Thought { turn, content } => println!("[{turn}] 思考：{content}"),
                Step::Answer { turn, content } => println!("\n[{turn}] 答案：{content}"),
                Step::Action {
                    turn,
                    name,
                    arguments,
                } => println!("[{turn}] 调用：{name} 参数 {arguments}"),
                Step::Observation { turn, name, output } => {
                    println!("[{turn}] {name} 返回：{output}");
                }
            },
            &mut |_, _| {},
        )
        .await?;

    println!(
        "\n---终止于 {:?}，共 {} 轮 ---",
        outcome.termination, outcome.turns
    );

    Ok(())
}
