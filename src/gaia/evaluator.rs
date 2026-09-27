use crate::gaia::{
    models::{GaiaEvalResult, GaiaRow},
    solver::{GAIA_PROMPT, solve_gaia_question_with_retry},
};

pub fn is_correct(predicate: &str, answer: &str) -> bool {
    let predicate = predicate.trim().to_lowercase();
    let answer = answer.trim().to_lowercase();
    predicate == answer
}

pub async fn evaluate_gaia(problom: GaiaRow, model_id: &str) -> GaiaEvalResult {
    let result = solve_gaia_question_with_retry(model_id, GAIA_PROMPT, &problom.question).await;
    match result {
        Ok(output) => GaiaEvalResult {
            task_id: problom.task_id,
            model: model_id.to_string(),
            correct: is_correct(&output.final_answer, &problom.final_answer),
            is_solvable: Some(output.is_solvable),
            prediction: Some(output.final_answer.clone()),
            answer: problom.final_answer,
            unsolvable_reason: Some(output.unsolvable_reason),
            error: None,
        },
        Err(err) => GaiaEvalResult {
            task_id: problom.task_id,
            model: model_id.to_string(),
            correct: false,
            is_solvable: None,
            prediction: None,
            answer: problom.final_answer,
            unsolvable_reason: None,
            error: Some(err.to_string()),
        },
    }
}
