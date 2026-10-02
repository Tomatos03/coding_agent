# 构建与运行

```bash
cargo build
cargo run                          # bin: coding_agent —— History + LLMClient::complete 的单轮 demo（main.rs）
cargo run --bin gaia               # GAIA Level 1 对比评测：每题各跑一次「带工具 / 不带工具」，输出两组通过数/通过率；需要 HF_TOKEN
cargo run --example react_chat     # ReAct 主循环，逐轮打印 Thought / Answer / Action / Observation（非流式）
cargo run --example stream_chat    # 流式输出
cargo run --example semaphore_chat # 并发限流：5 个任务抢 3 个 permit
cargo run --example web_search     # 裸 HTTP 打 Tavily + 响应解析
cargo run --example tool_exec      # 不走 LLM，直接验证「注册表 → trait 对象 → execute」
cargo run --example mcp_probe      # 连接 stdio MCP server，打印/调用适配出的工具（默认用测试 fixture）
cargo run --example mcp_react      # 端到端：用户提问 → ReAct → 调用 MCP（脚本化模型，无需 LLM 凭证）
cargo run --example mcp_chat       # 真实 LLM + MCP：从 mcp.json 加载工具，模型自主调用（需要凭证与 mcp.json）
cargo run --example rag_chat       # 端到端检索：ingest 若干文本 → 提问 → 打印 top-k（需 EMBEDDING_* 凭证）
cargo fmt
cargo clippy                       # 当前 -- -D warnings 下零告警
```

`react_chat`、`stream_chat`、`mcp_chat` 需要 LLM provider 的凭证（`mcp_chat` 还需要 `mcp.json`）；`web_search` 与 `tool_exec` 另外需要 Tavily 的凭证；`mcp_probe` 与 `mcp_react` 用自带假 server 时不需要任何凭证（只需 `python3`）；`rag_chat` 需要 embedding 凭证（`EMBEDDING_*`）。

## 测试

**当前 96 个测试**：默认跑 92 个（全部离线，不联网、不需要凭证），另外 4 个是 `#[ignore]`：3 个 MCP 集成测试（需要本机 `python3`）+ 1 个 embedding 真实端点联测（需要 `EMBEDDING_*` 凭证）。

`src/agent/react/runner.rs` 17 个，覆盖循环逻辑：

- 调 `final_answer` → `Termination::FinalAnswer`，答案取 `execute` 的返回值，且该调用有配对 tool 消息
- 纯文本回复（无 tool_calls）→ 端点无视了 `required`，降级为 `Termination::ModelFinished`；同时断言循环内收到的策略是 `Required`
- 一轮工具后调 `final_answer` → tool 消息按序进了历史
- 工具执行失败 → 循环没断，错误进了 Observation
- 未知工具名 → 同上
- `final_answer` 参数非法 → 压成 Observation、本轮不终止，下一轮重试后才收尾
- 同轮既调别的工具又调 `final_answer` → 以 `final_answer` 为准，且**每个** call 都有配对 tool 消息
- 连续请求工具 → 撞上限，`Termination::MaxTurns`，收尾轮**用 `Force("final_answer")` 强制**并发出 `Step::Answer`；断言完整策略序列 `[Required, Required, Force(...)]`
- 撞上限的收尾轮**只把 `final_answer` 暴露给模型**（工具面裁剪），断言两次请求的工具名序列
- 端点无视具名强制、收尾轮只回 `content` → 退回把 `content` 当答案；`content` 也空 → 交付固定兜底文案，**不报错**
- 收尾轮的 `final_answer` 参数非法 → 一样软着陆（退回 `content`/兜底文案），不因收尾失败丢掉整轮结果
- 模型返回空回复（content 与 tool_calls 都为空）→ 立即以 `Termination::EmptyReply` 收束，`turns` 为当前轮，历史末尾留下一条空 assistant 消息
- `execute()` 把工具失败与未知工具名压成观察文案（「工具执行失败：…」/「未知工具：…」），并能执行 `final_answer`（输入即输出）
- **收尾轮发 `Step::Answer` 而非 `Thought`** → 断言完整事件序列 `[(1,action),(1,observation),(2,answer)]`
- **中间轮的 content 发 `Step::Thought`** → 断言 `[(1,thought),(1,action),(1,observation),(2,answer)]`

后两条用 `trace()` 辅助函数把 `Step` 压成 `(轮次, 类型)` 序列做整体比对——比逐个 `assert!(matches!(...))` 更能钉住**顺序**，而这两条的核心正是发射顺序。

`src/agent/react/context.rs` 6 个：`ExecuteContext` 每次构造拿到唯一 id 且初始 `Running`、`set_status` 的状态流转、`Event` 记录 name/content/role/timestamp、事件序列化为平铺 JSON（毫秒时间戳）、`set_turn` 只保留当前轮事件、事件保持插入顺序。

`src/agent/llm/models.rs` 9 个：

- 流式分片重组（`ToolCallAccumulator`）3 个：单个调用的 `arguments` 被切成 4 片 → 拼回完整字符串；两个调用的分片交错到达 → 各归各的槽位；中间有空槽位 → `finish()` 丢掉没拿到 `name` 的
- 策略 → 请求体 3 个（`build_chat_request` 纯函数，序列化后断言）：`Required` → `tool_choice == "required"` 且 `parallel_tool_calls == true`；`Force("final_answer")` → `tool_choice == {"type":"function",...}`；**`tools == None` 时 `tool_choice` 键必须消失**（否则 `required` + 零工具会被服务端 400 拒绝）
- `tool_choice` 被拒判定 3 个：400 + `param:"tool_choice"`（或消息里点名）命中；429 / 500 / 不相关的 400 不命中；只有非 `Auto` 策略才值得重发

`src/tools/local/final_answer/mod.rs` 5 个：`execute` 把输入参数原样返回、可重复调用（纯函数）、`extract_answer` 容忍首尾空白、拒绝非法 JSON / 缺字段 / 空串、`execute` 传播解析错误。

MCP 相关共 32 个：

- `src/tools/mcp/config.rs` 10 个：配置解析、默认值、非法字段/名字/超时拒绝、缺文件回退
- `src/tools/mcp/tool.rs` 14 个（1 个 ignored）：结果映射、参数解析、命名校验、适配器端到端调用
- `src/tools/mcp/connection.rs` 5 个（1 个 ignored）：`Send + Sync`、空配置、失败隔离、真实 server 工具发现
- `src/tools/mod.rs` 3 个（1 个 ignored）：重名去重、空配置含 `web_search` + `final_answer` 两个本地工具、连接在注册后仍存活

GAIA 相关 11 个（全部离线）：`src/gaia/solver.rs` 6 个（严格 JSON / 代码块与正文包裹 / 字符串内花括号 / 纯文本兜底 / 空内容报错 / 无平衡对象），`src/gaia/report.rs` 2 个（按模型×模式汇总、通过率边界），`src/gaia/evaluator.rs` 2 个（脚本化 `Completer` 跑通带工具的 ReAct 路径并统计工具调用次数；**`final_answer` 不计入工具调用**），`src/gaia/models.rs` 1 个（`schemars` 的 `deny_unknown_fields` 只作用于 JSON Schema，serde 侧仍忽略未知字段）。

RAG 相关 16 个：`src/agent/rag/store.rs` 12 个（余弦五态：相同/平行/正交/相反/零向量；空向量与维度守卫；降序排序、top_k 截断/为 0、空库、查询维度不符），全部离线；`src/agent/rag/embed.rs` 4 个（1 个 ignored：请求体序列化、首条向量提取、空 data 报错；真实端点联测）。

4 个 `#[ignore]` 里，3 个 MCP 集成测试需要 `python3` + `tests/fixtures/fake_mcp_server.py`，1 个 embedding 联测需要 `EMBEDDING_*` 凭证；跑法都是：

```bash
cargo test --lib -- --ignored
```

能离线测的原因是把传输层抽象成了 `Completer` trait：测试用 `ScriptedCompleter`（预置响应队列）+ `EchoTool`（参数含 `boom` 就报错）替掉真实网络。MCP 侧同理——用 `tests/fixtures/` 下的假 server 替掉真实 MCP server。

**给新组件补测试时沿用这个模式**：先做一个 trait 接缝，再写假的实现。`Reply` 有 `Default` 且字段是 `String` / `Vec`，构造测试响应不需要任何辅助函数。

`tests/` 目录只放测试支撑资源（目前是 `tests/fixtures/fake_mcp_server.py`），没有 Rust 集成测试目标。`cargo test` **不**编译 `examples/`；要连示例一起校验得用 `cargo test --all-targets`（README 的跑法就是它）。

## 环境变量

`.env` 位于仓库根目录且已 gitignore。provider 与模型 ID 是**两个独立变量，必须配套**——用错组合表现为认证失败或 404，见 `@rules/architecture.md` 第 6 节。

| 变量 | 必需 | 用途 |
|---|---|---|
| `CURRENT_USE_PROVIDER` | 否 | 选择 provider：`openrouter`（默认）/ `deepseek`。缺失时回退到 `constant::provider::DEFAULT_PROVIDER` |
| `CURRENT_USE_MODEL_ID` | **是** | 当前 provider 对应的模型 ID。**缺失即报错，不设默认值**——这是刻意的，避免静默用错模型 |
| `OPENROUTER_API_BASE_URL` / `OPENROUTER_API_KEY` | 用 openrouter 时 | 该 provider 的 base 与认证 |
| `DEEPSEEK_API_BASE_URL` / `DEEPSEEK_API_KEY` | 用 deepseek 时 | 该 provider 的 base 与认证 |
| `HF_TOKEN` | 仅 `cargo run --bin gaia` | HuggingFace datasets-server |
| `GAIA_LIMIT` | 否 | GAIA 单轮题量，缺失或非法时回退到 `constant::gaia::DEFAULT_GAIA_LIMIT`（10） |
| `MAX_COMPLETION_TOKENS` | 否 | 每次请求的输出 token 上限，缺失或非法时回退到 `constant::provider::DEFAULT_MAX_TOKENS`（**8192**）。**推理模型要显著调大**——reasoning 分片同样计入这个预算，见 `@rules/architecture.md` 第 6 节 |
| `TAVILY_API_KEY` | 仅 web_search 工具 | Tavily 搜索接口认证，变量名常量在 `constant::search` |
| `EMBEDDING_API_BASE_URL` / `EMBEDDING_API_KEY` / `EMBEDDING_MODEL_ID` | 仅 rag 模块与 `rag_chat` 示例 | embedding 端点、认证与模型 ID，三个都必填；**不随 `CURRENT_USE_PROVIDER` 切换**。变量名常量在 `constant::embedding` |

可用的 provider 及各自的变量名在 `src/constant/provider.rs` 的 `PROVIDER_BASE_URL_VARS` 中定义——新增一个 provider 要改那里。

`dotenv` **不覆盖已存在的环境变量**：若 shell 里已 `export CURRENT_USE_MODEL_ID=...`，改 `.env` 不生效，排查时先 `echo` 一下。

MCP server 配置走**文件** `mcp.json`（位于当前工作目录，当前不支持用环境变量覆盖路径），格式与错误语义见仓库根 `README.md`。

## 引导与日志

环境与日志统一走 `bootstrap::init()`（`src/bootstrap.rs`：`dotenv` + `tracing_subscriber`）。`env()` 会区分「`.env` 不存在」（正常，打 `warn`）与「解析失败」（异常，同样打 `warn` 但不吞掉原因）。`logging()` 用 `try_init()`，重复调用安全，但**不要**在模块内另行 `dotenv::dotenv()`——已被 `bootstrap::init()` 覆盖，重复调用徒增噪音。

日志级别固定为 `INFO`（`src/bootstrap.rs` 的 `logging()` 里写死 `with_max_level`），没有通过 `RUST_LOG` 覆盖的入口；需要临时降噪/升噪时改该处。MCP 相关日志统一打了 `target: "mcp"`，但同样受这个级别限制——server 的 `stderr`（`debug`）默认看不到。

## 工具链

无 `rust-toolchain.toml`，也没接 CI。`edition = "2024"` 需要 rustc ≥ 1.85（实际用到 let-chains 与 `Result::inspect_err`，需要 ≥ 1.88 / 1.76）。

`Cargo.toml` 里的 `async-stream` 目前**没有任何调用方**，属于待清理项（`uuid` 已被 `src/agent/react/context.rs` 使用）。
