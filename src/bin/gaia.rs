use std::sync::Arc;

use coding_agent::{
    agent::llm::{models::LLMClient, provider, semaphore::get_semaphore},
    gaia::{
        dataset::load_gaia_level1,
        evaluator::{evaluate_gaia_with_tools, evaluate_gaia_without_tools},
        models::{GaiaEvalResult, GaiaMode},
        report::summarize,
    },
    tools::build_tools,
};
use tokio::task::JoinSet;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    coding_agent::bootstrap::init();
    gaia_level1_experiment().await
}

async fn gaia_level1_experiment() -> anyhow::Result<()> {
    let model = provider::model_id()?;
    let problems = load_gaia_level1().await?;

    // 工具表只建一次（MCP 工具会拉起子进程），再按题克隆共享。
    let tools = build_tools().await?;
    let mut tool_names: Vec<&str> = tools.keys().map(String::as_str).collect();
    tool_names.sort_unstable();
    tracing::info!(
        "GAIA 对比评测：{} 题，模型 {model}，工具 {tool_names:?}",
        problems.len()
    );

    let llm: Arc<LLMClient> = Arc::new(LLMClient::from_model(&model)?);

    let mut set = JoinSet::new();
    for problem in problems.iter().cloned() {
        let model = model.clone();
        let tools = tools.clone();
        let llm = Arc::clone(&llm);
        set.spawn(async move {
            let permit = get_semaphore().acquire().await?;
            let without_tools = evaluate_gaia_without_tools(problem.clone(), &model).await;
            let with_tools = evaluate_gaia_with_tools(problem, &model, llm, tools).await;
            drop(permit);
            Ok::<_, anyhow::Error>((without_tools, with_tools))
        });
    }

    let mut results: Vec<GaiaEvalResult> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok((without_tools, with_tools))) => {
                tracing::info!("{without_tools:#?}");
                tracing::info!("{with_tools:#?}");
                results.push(without_tools);
                results.push(with_tools);
            }
            Ok(Err(err)) => tracing::error!("Error evaluating problem: {:?}", err),
            Err(join_err) => tracing::error!("task 异常终止（panic 或取消）: {join_err:?}"),
        }
    }

    report(&results);
    Ok(())
}

fn report(results: &[GaiaEvalResult]) {
    let summaries = summarize(results);
    let mut current_model: Option<&str> = None;

    for summary in &summaries {
        if current_model != Some(summary.model.as_str()) {
            tracing::info!("==== GAIA 结果对比（模型：{}）====", summary.model);
            current_model = Some(&summary.model);
        }

        let tool_note = if summary.mode == GaiaMode::WithTools {
            format!(
                "（共 {} 次工具调用，{}/{} 题用到工具）",
                summary.tool_calls, summary.tasks_with_tool_calls, summary.total
            )
        } else {
            String::new()
        };

        tracing::info!(
            "{}：通过 {}/{}，通过率 {:.2}%{}",
            summary.mode.label(),
            summary.correct_count,
            summary.total,
            summary.pass_rate(),
            tool_note
        );
    }
}
