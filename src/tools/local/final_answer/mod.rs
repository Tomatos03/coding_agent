//! 最终答案工具：把「交付答案」抽象成一次普通的工具调用。
//!
//! 与其它工具的区别只在语义：`execute` **输入即输出**——把 `answer` 参数解析后
//! 原样返回。于是 ReAct 循环不必为它开特殊分支：走统一的 execute → Observation
//! 路径就能拿到最终答案，同时天然满足「每个 tool_call 都有配对 tool 消息」。
//!
//! `extract_answer` 是纯函数且无副作用，循环正是靠它「必有确定返回值」来做终止判定；
//! 给这个工具加副作用会破坏那条隐含约定。

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::tool::Tool;

pub const FINAL_ANSWER_TOOL: &str = "final_answer";

#[derive(Debug, Clone, JsonSchema, Serialize, Deserialize)]
pub struct FinalAnswerArgs {
    #[schemars(description = "交付给用户的最终答案，必须直接可读、不包含额外解释。")]
    pub answer: String,
}

pub struct FinalAnswer;

/// 解析工具调用参数里的 `answer`。
///
/// 纯函数：非法 JSON / 缺字段 / 空串都返回 `Err`，由调用方决定降级方式——循环会把它
/// 压成 Observation，让模型下一轮自己重试。
pub fn extract_answer(arguments: &str) -> anyhow::Result<String> {
    let args: FinalAnswerArgs = serde_json::from_str(arguments).map_err(|e| {
        anyhow::anyhow!("[{FINAL_ANSWER_TOOL}] Failed to deserialize arguments: {e}")
    })?;

    let answer = args.answer.trim();
    if answer.is_empty() {
        anyhow::bail!("[{FINAL_ANSWER_TOOL}] `answer` 不能为空");
    }

    Ok(answer.to_owned())
}

#[async_trait::async_trait]
impl Tool for FinalAnswer {
    fn name(&self) -> &str {
        FINAL_ANSWER_TOOL
    }

    fn description(&self) -> &str {
        "任务完成、准备交付最终答案时调用。调用后本轮任务立即结束，`answer` 即最终交付内容。\
         不要用它提问或请求确认。"
    }

    fn parameters(&self) -> Value {
        // schema 是静态的，序列化失败属于编程错误。
        serde_json::to_value(schema_for!(FinalAnswerArgs))
            .expect("failed to serialize FinalAnswerArgs schema")
    }

    async fn execute(&self, args_json: &str) -> anyhow::Result<String> {
        extract_answer(args_json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_returns_its_argument() {
        let answer = FinalAnswer
            .execute(r#"{"answer":"42"}"#)
            .await
            .expect("合法参数应当成功");

        assert_eq!(answer, "42");
    }

    #[tokio::test]
    async fn execute_is_repeatable_and_pure() {
        let tool = FinalAnswer;
        let args = r#"{"answer":"一样的答案"}"#;

        assert_eq!(
            tool.execute(args).await.unwrap(),
            tool.execute(args).await.unwrap()
        );
    }

    #[test]
    fn extract_trims_surrounding_whitespace() {
        assert_eq!(
            extract_answer(r#"{"answer":"  帕里斯  "}"#).unwrap(),
            "帕里斯"
        );
    }

    #[test]
    fn rejects_malformed_missing_and_empty_answers() {
        assert!(extract_answer("not json").is_err());
        assert!(extract_answer("{}").is_err());
        assert!(extract_answer(r#"{"answer":""}"#).is_err());
        assert!(extract_answer(r#"{"answer":"   "}"#).is_err());
    }

    #[tokio::test]
    async fn execute_propagates_extract_errors() {
        assert!(FinalAnswer.execute("not json").await.is_err());
    }
}
