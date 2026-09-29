use coding_agent::{
    agent::{
        llm::{
            models::{Completer, LLMClient, ToolPolicy},
            semaphore::get_semaphore,
        },
        react::history::History,
    },
    bootstrap::init,
    constant::prompt::SYSTEM_PROMPT,
};

const TASKS: usize = 5;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init();

    let mut set = tokio::task::JoinSet::new();

    for i in 0..TASKS {
        set.spawn(async move {
            let permit = get_semaphore().acquire().await?;

            let mut history = History::new();
            history.system(SYSTEM_PROMPT)?;
            history.user(&format!("我最喜欢的数字是 {i}，请围绕它规划一件小事。"))?;

            let client = LLMClient::new();
            let reply = client
                .stream(
                    history.as_slice(),
                    None,
                    // 没声明工具，策略被忽略。
                    ToolPolicy::Auto,
                    &mut |token| print!("{token}"),
                )
                .await?;

            drop(permit);
            anyhow::Ok(reply.content)
        });
    }

    while let Some(result) = set.join_next().await {
        match result {
            Ok(Ok(text)) => println!("\n--- 完成，{} 字符 ---", text.chars().count()),
            Ok(Err(e)) => tracing::error!("任务失败：{e}"),
            Err(join_err) => tracing::error!("任务 panic 或被取消：{join_err}"),
        }
    }

    Ok(())
}
