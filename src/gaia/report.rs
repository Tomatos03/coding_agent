use std::collections::BTreeMap;

use crate::gaia::models::{GaiaEvalResult, GaiaMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GaiaModeSummary {
    pub model: String,
    pub mode: GaiaMode,
    pub correct_count: usize,
    pub total: usize,
    pub tool_calls: usize,
    pub tasks_with_tool_calls: usize,
}

impl GaiaModeSummary {
    pub fn pass_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.correct_count as f64 / self.total as f64 * 100.0
        }
    }
}

/// 按 (模型, 模式) 汇总通过数、总数与工具调用量，供「带工具 / 不带工具」对比输出。
pub fn summarize(results: &[GaiaEvalResult]) -> Vec<GaiaModeSummary> {
    // 值依次是：(通过数, 总数, 工具调用次数, 用过工具的题数)
    let mut grouped: BTreeMap<(String, GaiaMode), (usize, usize, usize, usize)> = BTreeMap::new();

    for result in results {
        let entry = grouped
            .entry((result.model.clone(), result.mode))
            .or_default();
        entry.1 += 1;
        if result.correct {
            entry.0 += 1;
        }
        let calls = result.tool_calls.unwrap_or(0);
        entry.2 += calls;
        if calls > 0 {
            entry.3 += 1;
        }
    }

    grouped
        .into_iter()
        .map(
            |((model, mode), (correct_count, total, tool_calls, tasks_with_tool_calls))| {
                GaiaModeSummary {
                    model,
                    mode,
                    correct_count,
                    total,
                    tool_calls,
                    tasks_with_tool_calls,
                }
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(
        model: &str,
        mode: GaiaMode,
        correct: bool,
        tool_calls: Option<usize>,
    ) -> GaiaEvalResult {
        GaiaEvalResult {
            task_id: "t".to_owned(),
            model: model.to_owned(),
            mode,
            correct,
            is_solvable: None,
            prediction: None,
            answer: String::new(),
            unsolvable_reason: None,
            error: None,
            tool_calls,
        }
    }

    #[test]
    fn splits_counts_by_mode_and_model() {
        let results = vec![
            result("m1", GaiaMode::WithoutTools, true, None),
            result("m1", GaiaMode::WithoutTools, false, None),
            result("m1", GaiaMode::WithTools, true, Some(2)),
            result("m1", GaiaMode::WithTools, true, Some(0)),
            result("m2", GaiaMode::WithoutTools, false, None),
        ];

        let summaries = summarize(&results);

        assert_eq!(
            summaries,
            vec![
                GaiaModeSummary {
                    model: "m1".to_owned(),
                    mode: GaiaMode::WithoutTools,
                    correct_count: 1,
                    total: 2,
                    tool_calls: 0,
                    tasks_with_tool_calls: 0,
                },
                GaiaModeSummary {
                    model: "m1".to_owned(),
                    mode: GaiaMode::WithTools,
                    correct_count: 2,
                    total: 2,
                    tool_calls: 2,
                    tasks_with_tool_calls: 1,
                },
                GaiaModeSummary {
                    model: "m2".to_owned(),
                    mode: GaiaMode::WithoutTools,
                    correct_count: 0,
                    total: 1,
                    tool_calls: 0,
                    tasks_with_tool_calls: 0,
                },
            ]
        );
    }

    #[test]
    fn pass_rate_handles_empty_group_and_full_marks() {
        let empty = GaiaModeSummary {
            model: "m".to_owned(),
            mode: GaiaMode::WithTools,
            correct_count: 0,
            total: 0,
            tool_calls: 0,
            tasks_with_tool_calls: 0,
        };
        let full = GaiaModeSummary {
            model: "m".to_owned(),
            mode: GaiaMode::WithTools,
            correct_count: 3,
            total: 3,
            tool_calls: 0,
            tasks_with_tool_calls: 0,
        };

        assert_eq!(empty.pass_rate(), 0.0);
        assert_eq!(full.pass_rate(), 100.0);
    }
}
