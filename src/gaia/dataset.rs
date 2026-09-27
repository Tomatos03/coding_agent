use anyhow::Ok;

use crate::constant::gaia::{DEFAULT_GAIA_LIMIT, GAIA_LIMIT_ENV, HF_TOKEN_ENV};
use crate::gaia::models::{GaiaRow, HfResponse};

/// 读取 [`GAIA_LIMIT_ENV`]，缺失或非法时回退到 [`DEFAULT_GAIA_LIMIT`]。
///
/// 非数字值也走回退而非报错：这是可选调参项，不该因为写错一个环境变量
/// 就让整轮评测起不来（与 `provider::current_provider` 的处理一致）。
fn gaia_limit() -> usize {
    std::env::var(GAIA_LIMIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_GAIA_LIMIT)
}

pub async fn load_gaia_level1() -> anyhow::Result<Vec<GaiaRow>> {
    let token = std::env::var(HF_TOKEN_ENV)?;
    let limit = gaia_limit().to_string();
    tracing::info!("加载 GAIA 2023_level1/validation 前 {limit} 题");

    let client = reqwest::Client::new();
    let response = client
        .get("https://datasets-server.huggingface.co/rows")
        .query(&[
            ("dataset", "gaia-benchmark/GAIA"),
            ("config", "2023_level1"),
            ("split", "validation"),
            ("offset", "0"),
            ("length", limit.as_str()),
        ])
        .bearer_auth(token)
        .send()
        .await?
        .json::<HfResponse>()
        .await?;
    Ok(response.rows.into_iter().map(|r| r.row).collect())
}
