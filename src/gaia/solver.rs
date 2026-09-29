use std::sync::Arc;

use async_openai::types::chat::{
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
    CreateChatCompletionRequestArgs, FinishReason, ResponseFormat, ResponseFormatJsonSchema,
};
use backon::{ExponentialBuilder, Retryable};

use crate::agent::llm::{models::Completer, provider};
use crate::agent::react::models::{DEFAULT_MAX_TURNS, Step};
use crate::agent::react::runner::ReactLoop;
use crate::gaia::models::GaiaOutput;
use crate::tools::ToolHashMap;

pub const GAIA_PROMPT: &str = r#"You are a general AI assistant. I will ask you a question.
First, determine if you can solve this problem with your current capabilities and set "is_solvable" accordingly.
If you can solve it, set "is_solvable" to true and provide your answer in "final_answer".
If you cannot solve it, set "is_solvable" to false and explain why in "unsolvable_reason".
Your final answer should be a number OR as few words as possible OR a comma-separated list of numbers and/or strings.
If you are asked for a number, don't use a comma to write your number neither use units such as $ or percent sign unless specified otherwise.
If you are asked for a string, don't use articles, neither abbreviations (e.g., for cities), and write the digits in plain text.
If you are asked for a comma-separated list, apply the above rules depending on whether the element is a number or a string.
Respond with a single JSON object containing exactly these keys: "is_solvable" (boolean), "unsolvable_reason" (string, empty when is_solvable is true), "final_answer" (string).
"#;

pub const GAIA_TOOLS_PROMPT: &str = r#"You are a general AI assistant with access to tools. I will ask you a question.
Use the provided tools whenever they can help you gather facts or compute the answer (for example web_search). Call a tool only when it is useful; otherwise work it out yourself.
Once you have enough information, stop calling tools and reply with the final answer, and nothing else, as the JSON object described below.
Your final answer should be a number OR as few words as possible OR a comma-separated list of numbers and/or strings.
If you are asked for a number, don't use a comma to write your number neither use units such as $ or percent sign unless specified otherwise.
If you are asked for a string, don't use articles, neither abbreviations (e.g., for cities), and write the digits in plain text.
If you are asked for a comma-separated list, apply the above rules depending on whether the element is a number or a string.
First determine if the question is solvable with your current capabilities and set "is_solvable" accordingly.
When you are done, respond with a single JSON object containing exactly these keys: "is_solvable" (boolean), "unsolvable_reason" (string, empty when is_solvable is true), "final_answer" (string).
"#;

pub async fn solve_gaia_question_with_retry(
    model_id: &str,
    system: &str,
    prompt: &str,
) -> anyhow::Result<GaiaOutput> {
    let op = || async { solve_gaia_question(model_id, system, prompt).await };
    op.retry(ExponentialBuilder::default().with_max_times(3))
        .await
}

pub async fn solve_gaia_question_with_tools_retry(
    completer: &Arc<dyn Completer>,
    tools: &ToolHashMap,
    system: &str,
    prompt: &str,
) -> anyhow::Result<(GaiaOutput, usize)> {
    let op = || async {
        solve_gaia_question_with_tools(completer.clone(), tools.clone(), system, prompt).await
    };
    op.retry(ExponentialBuilder::default().with_max_times(3))
        .await
}

/// 用 ReAct 循环解题，返回解析后的输出与**实际发起的工具调用次数**。
///
/// 调用次数用于判断「带工具」这一组成绩是否真的用上了工具——模型可能全程
/// 直接作答，此时与「不带工具」的差异只剩输出格式，不代表工具起了作用。
pub async fn solve_gaia_question_with_tools(
    completer: Arc<dyn Completer>,
    tools: ToolHashMap,
    system: &str,
    prompt: &str,
) -> anyhow::Result<(GaiaOutput, usize)> {
    let mut agent = ReactLoop::new(completer, tools, system, DEFAULT_MAX_TURNS)?;

    let mut tool_calls = 0usize;
    let outcome = agent
        .run(
            prompt,
            |step| {
                if matches!(step, Step::Action { .. }) {
                    tool_calls += 1;
                }
            },
            |_, _| {},
        )
        .await?;

    let output = parse_gaia_output(&outcome.answer)?;
    Ok((output, tool_calls))
}

pub async fn solve_gaia_question(
    model_id: &str,
    system: &str,
    prompt: &str,
) -> anyhow::Result<GaiaOutput> {
    let response_format = build_response_format()?;
    let format_setting = response_format;

    let client = async_openai::Client::with_config(provider::client_config()?);
    let request = build_request(format_setting, model_id, system, prompt)?;
    let response = client.chat().create(request).await?;

    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices returned from the model"))?;

    if choice.finish_reason == Some(FinishReason::ContentFilter) {
        return Ok(GaiaOutput {
            is_solvable: false,
            unsolvable_reason: "Model refuse to answer".to_string(),
            final_answer: String::new(),
        });
    }

    let content = choice.message.content.ok_or_else(|| {
        anyhow::anyhow!("No content returned from the model in the choice message")
    })?;

    parse_gaia_output(&content)
}

/// 把模型返回的文本解析成 [`GaiaOutput`]，容忍 JSON 被包在正文或 ``` 代码块里。
///
/// 直答模式能靠 `response_format` 约束成纯 JSON；带工具模式走 ReAct 循环，没有
/// `response_format` 可依赖，收尾轮的 content 可能带前缀说明，所以这里逐级放宽：
/// 整段解析 → 提取第一个平衡的 `{...}` → 兜底把整段文本当作答案。
pub fn parse_gaia_output(content: &str) -> anyhow::Result<GaiaOutput> {
    if let Ok(output) = serde_json::from_str::<GaiaOutput>(content) {
        return Ok(output);
    }

    if let Some(candidate) = extract_json_object(content)
        && let Ok(output) = serde_json::from_str::<GaiaOutput>(&candidate)
    {
        return Ok(output);
    }

    let fallback = content.trim();
    if fallback.is_empty() {
        anyhow::bail!("模型没有返回可解析的 GAIA 输出");
    }

    Ok(GaiaOutput {
        is_solvable: true,
        unsolvable_reason: String::new(),
        final_answer: fallback.to_owned(),
    })
}

/// 取文本里第一个花括号平衡的子串；字符串字面量内的花括号与转义不计入深度。
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();

    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + 1;
                    return Some(text[start..end].to_owned());
                }
            }
            _ => {}
        }
    }

    None
}

fn build_request(
    format_setting: ResponseFormat,
    model_id: &str,
    system: &str,
    prompt: &str,
) -> Result<async_openai::types::chat::CreateChatCompletionRequest, anyhow::Error> {
    let request = CreateChatCompletionRequestArgs::default()
        .model(model_id)
        .messages(vec![
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system)
                .build()?
                .into(),
            ChatCompletionRequestUserMessageArgs::default()
                .content(prompt)
                .build()?
                .into(),
        ])
        .response_format(format_setting)
        .build()?;
    Ok(request)
}

fn build_response_format() -> Result<ResponseFormat, anyhow::Error> {
    // DeepSeek 官方不支持 json_schema，只能退到 json_object：失去 schema 层面的
    // 约束，字段形状改由 `GAIA_PROMPT` 末尾那段 JSON 说明保证（DeepSeek 还要求
    // prompt 里出现 "json" 字样，缺了会直接 400）。
    if !provider::supports_json_schema() {
        return Ok(ResponseFormat::JsonObject);
    }

    let schema = schemars::schema_for!(GaiaOutput); // 生成 JSON Schema
    let schema_json = serde_json::to_value(&schema); // 将 JSON Schema 转换为 serde_json::Value
    let format_setting = ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: Some("The output format for the GAIA question solver.".to_string()),
            name: "gaia_output".to_string(),
            schema: schema_json?,
            strict: Some(true),
        },
    };
    Ok(format_setting)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(is_solvable: bool, reason: &str, answer: &str) -> String {
        json!({
            "is_solvable": is_solvable,
            "unsolvable_reason": reason,
            "final_answer": answer,
        })
        .to_string()
    }

    #[test]
    fn parses_strict_json() {
        let output = parse_gaia_output(&payload(true, "", "42")).expect("整段合法 JSON 应解析成功");

        assert!(output.is_solvable);
        assert_eq!(output.final_answer, "42");
        assert!(output.unsolvable_reason.is_empty());
    }

    #[test]
    fn parses_json_wrapped_in_prose_or_code_fence() {
        let content = format!(
            "我先说明一下：\n```json\n{}\n```\n以上。",
            payload(true, "", "Paris")
        );

        let output = parse_gaia_output(&content).expect("应按第一个平衡对象提取");

        assert_eq!(output.final_answer, "Paris");
    }

    #[test]
    fn braces_inside_string_literals_do_not_break_extraction() {
        let answer = r#"集合 {1, 2} 与 "引号" 以及 } 都不该影响深度"#;
        let content = format!("前缀 {}\n后缀", payload(true, "", answer));

        let output = parse_gaia_output(&content).expect("字符串内的花括号不应计入深度");

        assert_eq!(output.final_answer, answer);
    }

    #[test]
    fn plain_text_falls_back_to_final_answer() {
        let output = parse_gaia_output("  Paris  ").expect("无 JSON 时应整段兜底");

        assert!(output.is_solvable);
        assert_eq!(output.final_answer, "Paris");
        assert!(output.unsolvable_reason.is_empty());
    }

    #[test]
    fn blank_content_is_rejected() {
        assert!(parse_gaia_output("   \n\t ").is_err());
        assert!(parse_gaia_output("").is_err());
    }

    #[test]
    fn unbalanced_braces_yield_no_object_then_fallback() {
        let content = r#"答案是 {"is_solvable":true,"unsolvable_reason":"","final_answer":"x""#;
        assert!(extract_json_object(content).is_none());

        let output = parse_gaia_output(content).expect("无平衡对象时应走纯文本兜底");

        assert!(output.is_solvable);
        assert_eq!(output.final_answer, content.trim());
    }
}
