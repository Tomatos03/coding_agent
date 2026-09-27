use tracing_subscriber::util::SubscriberInitExt as _;

pub fn env() {
    match dotenv::dotenv() {
        Ok(path) => tracing::info!("已加载环境变量文件: {}", path.display()),
        // `.env` 不存在是正常情况（例如改用真实环境变量注入），但**解析失败不是**：
        // 旧写法 `.ok()` 把两者一起吞掉，故障会推迟到某个 `env::var` 处，
        // 表现为与 `.env` 内容不符的 "environment variable not found"。
        Err(err) => tracing::warn!(".env 未加载: {err}"),
    }
}

pub fn logging() {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .finish();

    if subscriber.try_init().is_ok() {
        tracing::info!("tracing initialized");
    }
}

pub fn init() {
    // 先起日志再读环境：反过来会让 `env()` 里的告警无处可去。
    logging();
    env();
}
