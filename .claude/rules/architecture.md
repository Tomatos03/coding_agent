# 架构约束与陷阱

以下每一条都需要同时读多个文件才能拼出来。第 1、2、6 节是需要照搬的既有模式，第 3、4、5 节属于「违反时不会报错、只会静默出错」的陷阱，第 8 节是 MCP 工具层的结构与边界。

## 1. 客户端构造只有一条路径：`provider::client_config()`

所有 `async_openai::Client` 都经 `src/agent/llm/provider.rs` 的 `client_config()` 构造：

```rust
let client = async_openai::Client::with_config(provider::client_config()?);
```

现被两处共用：`src/agent/llm/models.rs` 的 `LLMClient::new`、`src/gaia/solver.rs` 的 `solve_gaia_question`。

它按 `CURRENT_USE_PROVIDER`（缺失时回退到 `constant::provider::DEFAULT_PROVIDER`）在 `constant::provider::PROVIDER_BASE_URL_VARS` 中查出该 provider 对应的 `*_API_BASE_URL` 与 `*_API_KEY` 两个环境变量名，任一缺失都报错并指明是哪个 provider 的哪个变量。

**不要写成 `async_openai::Client::new()`**——那走的是 `OpenAIConfig::default()`，读的是 `async-openai` 自己的 `OPENAI_BASE_URL`，本项目 `.env` 里没有这个变量，会静默 fallback 到 `https://api.openai.com/v1`，表现为认证失败或 404，而不是「没读到 base url」这种直观报错。跨模块搬运代码时尤其注意。

另有一个类型层面的坑：`async_openai::Client<C: Config>` **有泛型参数且没有默认值**。存进结构体字段时必须写全 `async_openai::Client<OpenAIConfig>`，写成 `async_openai::Client` 会报 E0107（missing generics）。`Client` 是 `Clone` 的，内部只是 `reqwest::Client` + `Arc` + config，克隆会共享同一个连接池，所以放进结构体复用没有额外开销。

## 2. 结构化输出管线

固定四步：定义 `#[derive(Serialize, Deserialize, JsonSchema)]` 结构体 → `schemars::schema_for!` → 塞进 `ResponseFormat::JsonSchema { strict: true }` → 从 `choice.message.content` 手工 `serde_json::from_str`。**模型的 JSON 不会被 SDK 自动反序列化**，解析失败要自己包错。

仓库里现在只剩**一条** `ResponseFormat` 管线：`src/gaia/solver.rs` 的 `build_response_format`（对应 `GaiaOutput`）。它用 `serde_json::to_value(&schema)`，返回 `Result`，因此多一个 `?`。

另有**两条参数 schema 管线**，去向不同、形态相似，**不要互相搬运**：

- `src/tools/tool.rs` 的 `Tool::definition()` 默认实现，用 `self.parameters()`（各工具自己产出 `serde_json::Value`）填 `FunctionObject.parameters`。
- `src/tools/local/web_search/mod.rs` 的 `WebSearchArgs` 用 `#[derive(JsonSchema)]` + `#[schemars(...)]` 属性描述字段，同文件里 `schema_for!(WebSearchArgs)` 取值。

`ResponseFormat` 那条把 schema 交给**服务端**做约束；`FunctionObject.parameters` 那条只是随请求发给**模型看**的描述。两者产出长得很像，用途完全不同。

**不支持 `json_schema` 的 provider 必须降级，且不能只换 `ResponseFormat`。** DeepSeek 只认 `json_object`，而它额外要求 prompt 里出现 "json" 字样——缺了直接 400：

```
Prompt must contain the word 'json' in some form to use 'response_format' of type 'json_object'
```

`provider::schema_hint(&schema_json)` 就是为满足这个要求而生（那句 "Respond with a single JSON object..."）。**目前唯一的调用方是 `src/gaia/solver.rs`**——因为结构化输出已从 agent 主路径移除。以后在别处新加 `ResponseFormat::JsonObject` 时，记得同时把 hint 拼进 prompt，否则只在 DeepSeek 上炸。`provider::supports_json_schema()` 是配套的分支判断。

相关细节：`GaiaOutput` 标了 `#[schemars(deny_unknown_fields)]`，使 schema 对模型施加额外约束；`src/gaia/solver.rs` 的 `solve_gaia_question` 把 `FinishReason::ContentFilter` 单独映射成 `is_solvable: false` 而非报错，因为拒答是评测中的预期结果之一。

## 3. 并发闸门由调用方负责，不在传输层内部

`src/agent/llm/semaphore.rs` 的 `get_semaphore()` 返回进程级 `&'static Semaphore`（3 permits），但 `Completer` 的两个方法（`complete` / `stream`）**内部都不会去获取它**。限流是否生效完全取决于调用方。

正确样板有两处：`src/bin/gaia.rs` 的 `gaia_level1_experiment()`、`examples/semaphore_chat.rs`——在 `JoinSet` 的每个 task 内部 `get_semaphore().acquire().await?`，用完 `drop(permit)`。

新增批量调用（循环、`join_all`、`JoinSet`）时必须自己拿 permit，否则并发数不受控且**不会有任何提示**。

同一函数里回收 `JoinSet` 的写法是配套样板：显式 match `Ok(Ok(_))` / `Ok(Err(_))` / `Err(join_err)` 三种结果。**不要退回成 `while let Some(Ok(result)) = set.join_next().await`**——首个 `JoinError`（task panic 或取消）会让循环静默提前结束，在途结果一并丢弃，表现为「结果少了几条」而无任何提示。

## 4. 重试包在整个操作外层，不是断点续传

统一用 `backon::{ExponentialBuilder, Retryable}` + `with_max_times(3)`，闭包返回的是**完整的** future。

**目前只有一处实现了重试**：`src/gaia/solver.rs` 的 `solve_gaia_question_with_retry`。`Completer` 的两个方法都没有重试包装，需要时自己加。

给流式加重试前必读：失败重试会重跑整个 stream，而上一轮已经 `print!` 出去的内容不会回滚，用户会看到重复输出。目前 `Completer::stream` **不写任何历史**（传输层已完全无状态），所以不存在「同一条回复进历史两次」的问题——但**给流式加副作用时（落库、追加消息）要重新考虑这件事**。

## 5. ReAct 循环的三条不变量

`src/agent/react/runner.rs` 的 `ReactLoop::run` 是全仓库唯一实现「请求 → 工具调用 → 回填 → 再请求」的地方（`LLMClient` 曾经有过一份自己的闭环，已随 `chat()` 一起删除）。三条不变量都是**违反时不报错、只在运行期静默出错**的：

**① `execute` 返回 `String`，不返回 `Result<String>`。**

```rust
async fn execute(&self, name: &str, arguments: &str) -> String
```

三个分支（成功 / 工具失败 / 未知工具）都产出一条 `String`。这是 ReAct 的硬要求：**Observation 不能表示「执行失败」，只能是一条内容为错误的观察**。改成 `Result` 会逼调用方 `?`，一 `?` 就退化成「循环中止」——那就不是 ReAct 了。同一个道理，`Tool::execute` 在 trait 层仍是 `Result<String>`（工具自己该知道自己失败了），是**循环**负责把它压成 Observation。

**② 每个 `tool_call` 都必须有配对的 tool 消息，顺序与 `calls` 一致。**

漏掉任何一条，OpenAI / DeepSeek 会在**下一轮请求**时用 "tool_calls must be followed by tool messages" 拒绝——错误信息不会指向这里。同理，循环里任何一处中途 `return Err` 都会让 `History` 停在半途状态，所以「工具的失败」绝不能用 `?` 传播。

**③ 撞轮次上限走软收尾，不 `bail!`。**

`run()` 的 for 循环跑满后调用 `finalize()`：去掉 `tools` 再问一轮，明确要求「不要再调用任何工具」。直接报错会把整轮探索的成果扔掉。`finalize` 只有在收尾轮返回空内容时才报错。

其余细节：`ReactLoop::new` 目前直接按 `ToolHashMap` 的迭代顺序收集工具定义（`HashMap` 顺序不保证，跨进程/跨运行可能不同，请求内容因此**不是严格可复现的**——要复现需在 `new` 里按名字排序后再 `map(definition)`）；`Thought`（`content`）和 `Action`（`tool_calls`）**一起**写进历史，丢掉 `content` 就丢了 ReAct 里的思考环节；模型返回空回复（content 与 tool_calls 都为空）时直接以 `Termination::EmptyReply` 收束，不再 nudge 并继续。

**`Step::Thought` 与 `Step::Answer` 互斥，判据是 `calls.is_empty()`，两者都必须在那个判断之后发射。** `Thought` 曾经写在判断之前，结果是**收尾轮的最终答案被错标成 Thinking**——任何「思考画暗、答案画亮」的渲染都会画错。这条由 `final_turn_is_an_answer_not_a_thought` 与 `intermediate_turn_with_content_is_a_thought` 两个测试按轮次钉住。

**已知不对称**：撞上限时走的 `finalize()` 在 turn 循环之外，只发 token 不发 `Step::Answer`。所以「收到 `Answer`」只覆盖 `ModelFinished` 那条终止路径。

**`on_token` 的签名是 `FnMut(turn, &str)`——轮次号随 token 一起给。** 理由是「这一轮开始了」没有独立信号：所有 `Step` 都在请求**返回之后**才发，而轮次前缀必须赶在第一个 token **之前**打出来。消费方按「turn 变了就补前缀」惰性处理，就不会给没有 content 的轮次（模型直接发 tool_calls）留下悬空前缀。收尾轮的 token 标 `max_turns + 1`——那是它真实的请求序号，而 `Outcome.turns` 报的是 `max_turns`，两者差 1 是已知的计数口径问题。

**流式与「区分思考/答案」不可兼得，这是物理限制而非实现缺陷。** token 到达时，这一轮会不会有 `tool_calls` **还不知道**——`Thought` 与 `Answer` 的判据（`calls.is_empty()`）必须等请求结束才能求值。所以：

| 要什么 | 怎么用 | 代价 |
|---|---|---|
| 逐字输出 | `on_token` | 内容只能统一渲染，分不出思考与答案 |
| 分得清 | `on_step` | 内容整段到达，没有逐字效果 |

`ReactLoop::run` 同时接收 `on_step` 与 `on_token` 两个回调；`examples/react_chat.rs` 传入空的 `on_token`（只消费 `on_step`）——它要演示的是**循环结构**，而 `examples/stream_chat.rs` 已经覆盖了流式。想两者都要，得靠 ANSI 回写重打，属于渲染层的事，不该让 `Step` 去承担。

**流式下的 `tool_calls` 按 `index` 重组。** chunk 里的 `delta.tool_calls` 是分片到达的——`id` 和 `name` 通常只出现在第一片，`arguments` 被切成多片。`src/agent/llm/models.rs` 的 `ToolCallAccumulator` 负责拼接：按 `index` 找槽位（必要时 `resize_with` 补齐），逐片 `push_str`，`finish()` 时丢掉从没拿到 `name` 的空槽。

**改这段逻辑时注意**：`arguments` 是**字符串拼接**，不是 JSON 合并——中间态必然是非法 JSON，不能边收边解析。这条路径由 3 个单元测试覆盖（见 `commands.md` 的测试一节）。

## 6. provider 与模型 ID 必须配套

两者都来自环境变量，但由 `src/agent/llm/provider.rs` 分别读取，行为不同：

| | 读取函数 | 缓存 | 缺失时 |
|---|---|---|---|
| provider | `current_provider()` | **`OnceLock` 缓存**，运行时改环境变量不生效，需重启 | 回退到 `DEFAULT_PROVIDER` |
| 模型 ID | `model_id()` | 不缓存 | **明确报错，不设默认值** |

模型 ID 不硬编码是有意为之：它与 provider 强绑定且命名规则不同（OpenRouter 用 `<org>/<model>:<variant>`，DeepSeek 用裸名称），单个常量无法同时适配。

**换 provider 必须同步换 `CURRENT_USE_MODEL_ID`**，否则 provider 不认识该模型名，表现为认证失败或 404——错误信息不会告诉你「是 ID 和 provider 不配套」。

`src/constant/provider.rs` 里放的是**环境变量名与 provider 名的字面量**（`PROVIDER_ENV`、`MODEL_ID_ENV`、`DEFAULT_PROVIDER`、`PROVIDER_BASE_URL_VARS`），不是模型 ID 本身。`prompt.rs` 放 `SYSTEM_PROMPT`，`gaia.rs` 放 GAIA 评测参数。

`NVIDIA_NEMOTRON_3_ULTRA_550B_A55B` 是写死的样例模型 ID，**没有任何调用方**，保留仅为方便手工试验。

### 推理模型的 reasoning 分片会被静默丢弃

`async-openai 0.41` 的 `ChatCompletionStreamResponseDelta` **没有 `reasoning_content` 字段**：

```rust
pub struct ChatCompletionStreamResponseDelta {
    pub content: Option<String>,
    pub function_call: Option<FunctionCallStream>,   // deprecated
    pub tool_calls: Option<Vec<ChatCompletionMessageToolCallChunk>>,
    pub role: Option<Role>,
    pub refusal: Option<String>,
}
```

所以推理模型（`deepseek-reasoner`、OpenRouter 上的 R1 系等）在思考阶段的每一个分片，**你会收到、但 serde 找不到对应字段、直接丢掉**——既不进 `content`，也不报错。

**后果**：reasoning token **照样计入 `max_tokens` 预算**，但客户端完全看不见消耗。预算不够时表现为「请求成功、零报错、`content` 是空字符串」，`finish_reason` 为 `Length`。

实测过一次：`max_tokens: 2048` 时收到 2050 个分片、`content 0 字`、`finish_reason Some(Length)`——整份预算烧在看不见的推理上。

**排查**：`stream()` 只在「预算耗尽且零产出」（`finish_reason == Length` 且 content / tool_calls 都为空）时打一条 `warn`；原先那行「流式收束」INFO 日志已移除。预算由 `MAX_COMPLETION_TOKENS` 控制，默认 8192。

## 7. 外部密钥不进源码

规则：密钥一律走 `.env`（已 gitignore），代码里读环境变量，变量名常量放 `src/constant/<领域>.rs`。样板是 `src/tools/local/web_search/mod.rs`：值在 `execute` 里用 `std::env::var(TAVILY_API_KEY_ENV)` 读取，缺失时返回 `Err`（被 ReAct 循环压成 Observation）。**不在构造期读**——否则 `build_tools` 会依赖凭证，离线测试和不碰 web_search 的用法一并被拖垮。

关键在于 `gitignore` 只忽略了 `.env`、`mcp.json`、`.delta`、`/target`——**`src/` 下的文件会随 `git add` 进入历史**。任何写进源码的凭证都算已泄露，改代码不足以补救，必须去服务方吊销重发。

## 8. MCP 工具层（stdio）

`src/tools/mcp/` 通过 **stdio** 把本地子进程当作 MCP Server 接入，一期只支持 `tools`（`initialize` / `tools/list` / `tools/call`），不涉及 resources / prompts / sampling / HTTP。

- **角色**：本项目是 MCP Host；`connection.rs` 里每个 `RunningService<RoleClient, ()>` 是一个 MCP Client（与 server 1:1）；被启动的子进程是 Server。`()` 是 client 侧的 `ClientHandler`（无操作），因为一期不声明任何反向能力。
- **接入点**：`tools/mod.rs` 的 `build_tools()` 读 `mcp.json` → `connect_all()` → `tools_from_connection()`，与本地工具合并；`build_tools_with(config)` 是不读文件、可离线测试的接缝。**本地工具先注册，因此命名冲突时本地优先，MCP 工具被跳过并告警。**
- **连接实现**：不用 rmcp 的 `transport-child-process`（会引入 `process-wrap`），而是自己 `tokio::process::Command` 拉起子进程，取 `stdout` / `stdin` 后 `().serve((stdout, stdin))`；传输走 rmcp 的 `transport-async-rw`。`stderr` 必须起 task 持续读，否则管道写满会阻塞子进程。
- **keep-alive（最易错）**：`McpConnection` 持有 `_child: Child` 且 `kill_on_drop(true)`，**必须保存它**——drop 掉 `Child` 会立刻杀掉刚建好的子进程。`McpTool` 持有 `Arc<McpConnection>`，所以 `connect_all` 返回的 `Vec` 被 drop 后连接仍存活；这条由集成测试 `registry_keeps_mcp_connection_alive` 钉住。
- **超时**：握手/发现用固定 `HANDSHAKE_TIMEOUT_SECS`（30s）；每次 `tools/call` 用 server 配置的 `timeoutSecs`（默认 60s）。两者用途不同，不要混淆。
- **失败隔离**：`required=false` 的 server 连接失败只告警跳过；`required=true` 才让启动整体失败；连接成功但 0 工具会被丢弃并回收子进程。
- **工具命名**：暴露名 `{server}__{tool}`，必须满足 OpenAI function 名规则 `^[a-zA-Z0-9_-]{1,64}$`——**一个非法名会让整个 `ReactLoop::new` 失败**，所以非法/超长的工具在适配期就跳过并告警。
- **结果映射**：`render_call_result` 把 `CallToolResult` 降维成单个 `String`；非文本内容（图片/音频/资源）只放占位摘要，不塞 base64；`is_error=true` 仍返回 `Ok(文本)` 让模型自纠错，只有协议/传输/超时错误才 `Err`（再由 ReAct 循环压成 Observation）。
- **不支持**：MRTR（`InputRequired`）与 tasks（`Task`）响应直接报错；不做运行时工具列表热刷新，也不重连。子进程异常退出只能在**下一次调用**时报错暴露。
- **日志**：MCP 相关日志统一 `target: "mcp"`，便于过滤；但 `bootstrap.rs` 的 subscriber 固定 `with_max_level(INFO)`，`debug`（server stderr）默认不可见。
- **测试 fixture**：`tests/fixtures/fake_mcp_server.py` 是手写裸 JSON-RPC 的最小 server（`echo` / `add` / `fail`），3 个 `#[ignore]` 集成测试依赖本机 `python3`，跑法 `cargo test --lib -- --ignored`。
