use coding_agent::{
    agent::rag::{embed::Embedder, retriever::Retriever},
    bootstrap::init,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let embedder = Embedder::new()?;
    let mut retriever = Retriever::new(embedder);

    let documents = [
        "Rust 的所有权系统在编译期检查内存安全，不需要垃圾回收。",
        "ReAct 循环按「思考 → 行动 → 观察」交替推进，模型通过调用 final_answer 结束任务。",
        "余弦相似度等于两向量的点积除以两个模长的乘积，衡量方向一致性，取值在 -1 到 1 之间。",
        "MCP 通过 stdio 启动本地子进程作为工具服务器，本项目扮演 MCP Host。",
        "向量数据库的核心增值是 ANN 近似最近邻索引；数据量小时暴力全扫反而更简单。",
    ];

    for text in documents {
        retriever.ingest(text).await?;
    }
    println!("已入库 {} 条文档", retriever.len());

    let question = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ReAct 循环是怎么推进的？".to_owned());
    let hits = retriever.retrieve(&question, 3).await?;

    println!("\n问题：{question}\n");
    for hit in hits {
        println!("  {:.4}  {}", hit.score, hit.text);
    }

    Ok(())
}
