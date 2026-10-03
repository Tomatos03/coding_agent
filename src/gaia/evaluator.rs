use std::sync::Arc;

use crate::agent::llm::models::LLMClient;
use crate::gaia::{
    models::{GaiaEvalResult, GaiaMode, GaiaOutput, GaiaRow},
    solver::{
        GAIA_PROMPT, GAIA_TOOLS_PROMPT, solve_gaia_question_with_retry,
        solve_gaia_question_with_tools_retry,
    },
};
use crate::tools::ToolHashMap;

pub fn is_correct(predicate: &str, answer: &str) -> bool {
    let predicate = predicate.trim().to_lowercase();
    let answer = answer.trim().to_lowercase();
    predicate == answer
}

pub async fn evaluate_gaia_without_tools(problem: GaiaRow, model_id: &str) -> GaiaEvalResult {
    let result = solve_gaia_question_with_retry(model_id, GAIA_PROMPT, &problem.question).await;
    into_eval_result(&problem, model_id, GaiaMode::WithoutTools, None, result)
}

pub async fn evaluate_gaia_with_tools(
    problem: GaiaRow,
    model_id: &str,
    llm: Arc<LLMClient>,
    tools: ToolHashMap,
) -> GaiaEvalResult {
    let result =
        solve_gaia_question_with_tools_retry(&llm, &tools, GAIA_TOOLS_PROMPT, &problem.question)
            .await;

    let tool_calls = result.as_ref().ok().map(|(_, calls)| *calls);
    let output = result.map(|(output, _)| output);
    into_eval_result(&problem, model_id, GaiaMode::WithTools, tool_calls, output)
}

fn into_eval_result(
    problem: &GaiaRow,
    model_id: &str,
    mode: GaiaMode,
    tool_calls: Option<usize>,
    result: anyhow::Result<GaiaOutput>,
) -> GaiaEvalResult {
    match result {
        Ok(output) => GaiaEvalResult {
            task_id: problem.task_id.clone(),
            model: model_id.to_string(),
            mode,
            correct: is_correct(&output.final_answer, &problem.final_answer),
            is_solvable: Some(output.is_solvable),
            prediction: Some(output.final_answer.clone()),
            answer: problem.final_answer.clone(),
            unsolvable_reason: Some(output.unsolvable_reason),
            error: None,
            tool_calls,
        },
        Err(err) => GaiaEvalResult {
            task_id: problem.task_id.clone(),
            model: model_id.to_string(),
            mode,
            correct: false,
            is_solvable: None,
            prediction: None,
            answer: problem.final_answer.clone(),
            unsolvable_reason: None,
            error: Some(err.to_string()),
            tool_calls,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, FunctionCall,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::agent::llm::models::Reply;
    use crate::tools::local::final_answer::{FINAL_ANSWER_TOOL, FinalAnswer};
    use crate::tools::tool::Tool;

    /// 脚本化 `LLMClient`：按预置回复队列驱动带工具的评测。
    fn scripted(replies: Vec<Reply>) -> Arc<LLMClient> {
        Arc::new(LLMClient::scripted(replies))
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "回显。"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, _args_json: &str) -> anyhow::Result<String> {
            Ok("echoed".to_owned())
        }
    }

    fn tool_reply() -> Reply {
        Reply {
            content: String::new(),
            tool_calls: vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: "call_1".to_owned(),
                    function: FunctionCall {
                        name: "echo".to_owned(),
                        arguments: "{}".to_owned(),
                    },
                },
            )],
        }
    }

    /// required 下模型唯一的收尾方式：把 GAIA 的 JSON 对象作为 `answer` 参数传给 `final_answer`。
    fn answer_reply(final_answer: &str) -> Reply {
        Reply {
            content: String::new(),
            tool_calls: vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: "call_2".to_owned(),
                    function: FunctionCall {
                        name: FINAL_ANSWER_TOOL.to_owned(),
                        arguments: json!({
                            "answer": format!(
                                r#"{{"is_solvable":true,"unsolvable_reason":"","final_answer":"{final_answer}"}}"#
                            )
                        })
                        .to_string(),
                    },
                },
            )],
        }
    }

    fn problem() -> GaiaRow {
        GaiaRow {
            task_id: "t1".to_owned(),
            question: "q".to_owned(),
            level: "1".to_owned(),
            final_answer: "Paris".to_owned(),
        }
    }

    fn tools() -> ToolHashMap {
        let mut tools = ToolHashMap::new();
        tools.insert("echo".to_owned(), Arc::new(EchoTool) as Arc<dyn Tool>);
        // 手工拼表同样要显式注册 final_answer：收尾轮会以 tool_choice 具名强制它。
        tools.insert(
            FINAL_ANSWER_TOOL.to_owned(),
            Arc::new(FinalAnswer) as Arc<dyn Tool>,
        );
        tools
    }

    #[tokio::test]
    async fn with_tools_runs_loop_and_counts_calls() {
        let llm = scripted(vec![tool_reply(), answer_reply("Paris")]);

        let result = evaluate_gaia_with_tools(problem(), "m", llm, tools()).await;

        assert!(result.correct);
        assert_eq!(result.mode, GaiaMode::WithTools);
        assert_eq!(result.tool_calls, Some(1), "echo 是一次真正的工具使用");
        assert_eq!(result.prediction.as_deref(), Some("Paris"));
    }

    #[tokio::test]
    async fn final_answer_is_not_counted_as_a_tool_call() {
        // 模型全程没碰真工具，只靠 final_answer 收尾：口径应报 0 次工具使用。
        let llm = scripted(vec![answer_reply("Paris")]);

        let result = evaluate_gaia_with_tools(problem(), "m", llm, tools()).await;

        assert!(result.correct);
        assert_eq!(
            result.tool_calls,
            Some(0),
            "final_answer 是收尾动作，不该被算成一次工具使用"
        );
        assert_eq!(result.prediction.as_deref(), Some("Paris"));
    }
}
