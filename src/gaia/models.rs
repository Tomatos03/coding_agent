use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug)]
pub struct HfResponse {
    pub rows: Vec<HfRow>,
}

#[derive(Deserialize, Debug)]
pub struct HfRow {
    pub row: GaiaRow,
}

#[derive(Deserialize, Clone, Debug)]
pub struct GaiaRow {
    pub task_id: String,

    #[serde(rename = "Question")]
    pub question: String,

    #[serde(rename = "Level")]
    pub level: String,

    #[serde(rename = "Final answer")]
    pub final_answer: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[schemars(deny_unknown_fields)]
pub struct GaiaOutput {
    pub is_solvable: bool,
    pub unsolvable_reason: String,
    pub final_answer: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GaiaMode {
    WithoutTools,
    WithTools,
}

impl GaiaMode {
    pub fn label(self) -> &'static str {
        match self {
            GaiaMode::WithoutTools => "不调用工具",
            GaiaMode::WithTools => "调用工具",
        }
    }
}

#[allow(dead_code)]
#[derive(Serialize, Debug)]
pub struct GaiaEvalResult {
    pub task_id: String,
    pub model: String,
    pub mode: GaiaMode,
    pub correct: bool,
    pub is_solvable: Option<bool>,
    pub prediction: Option<String>,
    pub answer: String,
    pub unsolvable_reason: Option<String>,
    pub error: Option<String>,
    pub tool_calls: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_are_ignored_by_serde() {
        let output: GaiaOutput = serde_json::from_str(
            r#"{"is_solvable":true,"unsolvable_reason":"","final_answer":"42","extra":"ignored"}"#,
        )
        .expect("未知字段不应导致解析失败");

        assert_eq!(output.final_answer, "42");
    }
}
