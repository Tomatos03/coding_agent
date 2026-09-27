use std::collections::HashMap;

use coding_agent::{
    agent::llm::{provider, semaphore::get_semaphore},
    gaia::{dataset::load_gaia_level1, evaluator::evaluate_gaia, models::GaiaEvalResult},
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
    let mut set = JoinSet::new();
    for problem in problems.iter() {
        let problem = problem.clone();
        let model = model.clone();
        set.spawn(async move {
            let permit = get_semaphore().acquire().await?;
            let eval = evaluate_gaia(problem, &model).await;
            drop(permit);
            Ok::<_, anyhow::Error>(eval)
        });
    }

    let mut results: HashMap<String, Vec<GaiaEvalResult>> = HashMap::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(eval)) => {
                tracing::info!("{eval:#?}");
                results.entry(eval.model.clone()).or_default().push(eval);
            }
            Ok(Err(err)) => tracing::error!("Error evaluating problem: {:?}", err),
            Err(join_err) => tracing::error!("task 异常终止（panic 或取消）: {join_err:?}"),
        }
    }

    for (model_id, evals) in results.iter() {
        let correct_count = evals.iter().map(|e| e.correct).filter(|&c| c).count();
        let total = evals.len();
        tracing::info!(
            "Model: {}, Correct: {}/{}, Accuracy: {:.2}%",
            model_id,
            correct_count,
            total,
            (correct_count as f64 / total as f64) * 100.0
        );
    }

    Ok(())
}
