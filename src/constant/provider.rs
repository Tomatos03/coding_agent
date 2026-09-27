pub const NVIDIA_NEMOTRON_3_ULTRA_550B_A55B: &str = "nvidia/nemotron-3-ultra-550b-a55b:free";

pub const PROVIDER_ENV: &str = "CURRENT_USE_PROVIDER";
pub const DEFAULT_PROVIDER: &str = "openrouter";
pub const MODEL_ID_ENV: &str = "CURRENT_USE_MODEL_ID";

pub const MAX_TOKENS_ENV: &str = "MAX_COMPLETION_TOKENS";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

pub const OPENROUTER: &str = "openrouter";
pub const OPENROUTER_API_BASE_URL_ENV: &str = "OPENROUTER_API_BASE_URL";
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

pub const DEEPSEEK: &str = "deepseek";
pub const DEEPSEEK_API_BASE_URL_ENV: &str = "DEEPSEEK_API_BASE_URL";
pub const DEEPSEEK_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

pub const PROVIDER_BASE_URL_VARS: &[(&str, &str, &str)] = &[
    (
        OPENROUTER,
        OPENROUTER_API_BASE_URL_ENV,
        OPENROUTER_API_KEY_ENV,
    ),
    (DEEPSEEK, DEEPSEEK_API_BASE_URL_ENV, DEEPSEEK_API_KEY_ENV),
];
