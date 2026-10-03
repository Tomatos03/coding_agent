# 架构约束与陷阱

以下每一条都需要同时读多个文件才能拼出来。第 1、2、6 节是需要照搬的既有模式，第 3、4、5 节属于「违反时不会报错、只会静默出错」的陷阱，第 8 节是 MCP 工具层的结构与边界，第 9 节是 RAG 检索层（内存版）。

## 1. 客户端构造只有两条路径：`provider::client_config()` 与 `embedding_client_config()`

所有 `async_openai::Client` 都经 `src/llm/provider.rs` 构造，按用途分两条：

```rust
let client = async_openai::Client::with_config(provider::client_config()?);           // 对话
let client = async_openai::Client::with_config(provider::embedding_client_config()?); // embedding
```

`client_config()` 现被两处共用：`src/llm/models.rs` 的 `LLMClient`（构造真实后端时）、`src/gaia/solver.rs` 的 `solve_gaia_question`。

它按 `CURRENT_USE_PROVIDER`（缺失时回退到 `constant::provider::DEFAULT_PROVIDER`）在 `constant::provider::PROVIDER_BASE_URL_VARS` 中查出该 provider 对应的 `*_API_BASE_URL` 与 `*_API_KEY` 两个环境变量名，任一缺失都报错并指明是哪个 provider 的哪个变量。

`embedding_client_config()` 现由 `src/rag/embed.rs` 的 `Embedder::new` 独用，配套的模型 ID 由 `embedding_model_id()` 读取。两者直接读 `EMBEDDING_API_BASE_URL` / `EMBEDDING_API_KEY` / `EMBEDDING_MODEL_ID`（变量名常量在 `constant::embedding`），**不随 `CURRENT_USE_PROVIDER` 切换**。

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

`src/llm/semaphore.rs` 的 `get_semaphore()` 返回进程级 `&'static Semaphore`（3 permits），但 `LLMClient` 的两个方法（`complete` / `stream`）**内部都不会去获取它**。限流是否生效完全取决于调用方。

正确样板有两处：`src/bin/gaia.rs` 的 `gaia_level1_experiment()`、`examples/semaphore_chat.rs`——在 `JoinSet` 的每个 task 内部 `get_semaphore().acquire().await?`，用完 `drop(permit)`。

新增批量调用（循环、`join_all`、`JoinSet`）时必须自己拿 permit，否则并发数不受控且**不会有任何提示**。

同一函数里回收 `JoinSet` 的写法是配套样板：显式 match `Ok(Ok(_))` / `Ok(Err(_))` / `Err(join_err)` 三种结果。**不要退回成 `while let Some(Ok(result)) = set.join_next().await`**——首个 `JoinError`（task panic 或取消）会让循环静默提前结束，在途结果一并丢弃，表现为「结果少了几条」而无任何提示。

## 4. 重试包在整个操作外层，不是断点续传

统一用 `backon::{ExponentialBuilder, Retryable}` + `with_max_times(3)`，闭包返回的是**完整的** future。

**目前只有一处实现了重试**：`src/gaia/solver.rs` 的 `solve_gaia_question_with_retry`。`LLMClient` 的两个方法都没有重试包装，需要时自己加。

给流式加重试前必读：失败重试会重跑整个 stream，而上一轮已经 `print!` 出去的内容不会回滚，用户会看到重复输出。目前 `LLMClient::stream` **不写任何历史**（传输层已完全无状态），所以不存在「同一条回复进历史两次」的问题——但**给流式加副作用时（落库、追加消息）要重新考虑这件事**。

## 5. ReAct 循环的四条不变量

`src/react/runner.rs` 的 `ReactLoop::run` 是全仓库唯一实现「请求 → 工具调用 → 回填 → 再请求」的地方（真实后端曾经有过一份自己的闭环，已随 `chat()` 一起删除）。四条不变量都是**违反时不报错、只在运行期静默出错**的：

**① `execute` 返回 `String`，不返回 `Result<String>`。**

```rust
async fn execute(&self, name: &str, arguments: &str) -> String
```

三个分支（成功 / 工具失败 / 未知工具）都产出一条 `String`。这是 ReAct 的硬要求：**Observation 不能表示「执行失败」，只能是一条内容为错误的观察**。改成 `Result` 会逼调用方 `?`，一 `?` 就退化成「循环中止」——那就不是 ReAct 了。同一个道理，`Tool::execute` 在 trait 层仍是 `Result<String>`（工具自己该知道自己失败了），是**循环**负责把它压成 Observation。

**② 每个 `tool_call` 都必须有配对的 tool 消息，顺序与 `calls` 一致。**

漏掉任何一条，OpenAI / DeepSeek 会在**下一轮请求**时用 "tool_calls must be followed by tool messages" 拒绝——错误信息不会指向这里。同理，循环里任何一处中途 `return Err` 都会让 `History` 停在半途状态，所以「工具的失败」绝不能用 `?` 传播。

**③ 撞轮次上限走软收尾，不 `bail!`。**

`run()` 的 for 循环跑满后调用 `finalize()`：把工具面裁到只剩 `final_answer`、追加一条 system 指令，再用 `tool_choice={"type":"function","function":{"name":"final_answer"}}` 强制一轮，答案从工具参数里取。直接报错会把整轮探索的成果扔掉，所以 `finalize` **不 `bail!`**：参数非法、端点忽略具名强制、甚至 `content` 也空，一律按「`content` → 固定兜底文案」软着陆，并照常发出 `Step::Answer`。

**④ `final_answer` 是普通可执行工具，不是 `execute` 之前的特殊分支。**

`src/tools/local/final_answer/mod.rs` 的 `execute` **输入即输出**：解析 `answer` 参数后原样返回。于是循环用统一的 `action → execute → Observation` 路径就能拿到最终答案，每个 `tool_call` 也天然有配对 tool 消息（不变量②）。循环的终止判据是「参数可解析」（`extract_answer`），交付值取 `execute` 的返回值——两者同源、必然一致。**若改成「在 `execute` 之前拦截、跳过执行」，同轮其它 `tool_call` 就会失去配对 tool 消息。** `extract_answer` / `execute` 必须是纯函数：给这个工具加副作用会破坏「可安全重复调用」这条隐含约定。

**审批闸门（危险工具确认）在 `run_pending_calls()` 里、`Step::Action` 之后、`execute()` 之前。** `ReactLoop` 持有 `approval_policy`（来自工作区根目录的 `.agents/settings.json`，经 `src/settings.rs` 的 `load_settings()` 加载；缺文件 = 全默认 = 全放行）与可选 `confirmer`（`src/react/approval.rs` 的 `Confirmer` trait）。`action_for(name)` 判 `ask` 时经 `Confirmer` 拿决定：**拒绝被压成一条 Observation**（「用户拒绝执行工具…」）——它不是 `Err`、也不是 `execute` 之前的特例分支，而是与「工具失败 / 未知工具」完全相同的通道，不变量①②原样成立，消费方不用学新事件类型。两条静默陷阱：**(a)** 策略判 `ask` 但调用方未注入 `confirmer` 时**挂起**（`Termination::Suspended`），不再 fail-closed 直接拒绝——会话停在未执行完的工具批次上，等 session 层带决定 `resume`（`Confirmer` 主动返回 `Decision::Pending` 走同一条路；这是行为变更，详见第 11 节）；**(b)** 匹配只按**暴露名**做 glob（`*` 通配任意字符序列；`*__*` 恰命中一切 MCP 工具），参数级粒度（rm 拦、ls 放行）留给 `Confirmer` 自己看 `arguments`。豁免路径无需特判：`final_answer` 的交付路径与 `finalize()` 都不执行工具，结构上到不了闸门。`ReactLoop` 只收算好的 `ApprovalPolicy` / `Confirmer` 对象，自己不读文件——与「收 `ToolHashMap` 而不跑 `build_tools()`」同一条原则；配置启动时读一次，改完重启生效（与 `mcp.json` 一致）。

**循环内每轮都是 `tool_choice=required`。** 服务端保证回复里至少有一个 `tool_call`，所以模型**只能**靠 `final_answer` 结束，`content` 永远只是 thought。两条降级路径兜底：(a) 回复里没有 `tool_calls` → 端点无视了强制，把 `content` 当答案收尾（`Termination::ModelFinished`）；(b) 端点以 400 明确拒绝 `tool_choice` → 真实后端置粘性标记（`AtomicBool`）、改用 `Auto` 重发一次，此后所有请求都不再强制。`ToolPolicy`（`Auto` / `Required` / `Force(name)`）是请求级参数，随每轮传入 `LLMClient`，不能写进 messages。

其余细节：`ReactLoop::new` 目前直接按 `ToolHashMap` 的迭代顺序收集工具定义（`HashMap` 顺序不保证，跨进程/跨运行可能不同，请求内容因此**不是严格可复现的**——要复现需在 `new` 里按名字排序后再 `map(definition)`）；`Thought`（`content`）和 `Action`（`tool_calls`）**一起**写进历史，丢掉 `content` 就丢了 ReAct 里的思考环节；模型返回空回复（content 与 tool_calls 都为空）时直接以 `Termination::EmptyReply` 收束，不再 nudge 并继续。`final_answer` 的注册责任在**工具表构建方**：`build_tools*` 经 `local_tools()` 注册它；`ReactLoop::new` **不做兜底注入**，手工拼表的调用方必须自行插入，否则收尾轮的具名 `tool_choice` 会指向一个未声明的函数、被服务端拒绝。

**`Step::Thought` 与 `Step::Answer` 互斥，判据是 `calls.is_empty()`，两者都必须在那个判断之后发射。** `Thought` 曾经写在判断之前，结果是**收尾轮的最终答案被错标成 Thinking**——任何「思考画暗、答案画亮」的渲染都会画错。这条由 `final_turn_is_an_answer_not_a_thought` 与 `intermediate_turn_with_content_is_a_thought` 两个测试按轮次钉住。注意 `final_answer` 走的是交付路径：只发 `Step::Answer`，不执行、不发 `Step::Action`/`Observation`。

**`finalize()` 现在也发 `Step::Answer`。** 它曾经在 turn 循环之外、只发 token 不发 `Step`，导致只用 `on_step` 的消费方（如 `examples/react_chat`）在撞上限时看不到答案；`final_answer` 落地后收尾轮与正常轮走同一套收尾逻辑，这条不对称已消除。

**`on_token` 的签名是 `FnMut(turn, &str)`——轮次号随 token 一起给。** 理由是「这一轮开始了」没有独立信号：所有 `Step` 都在请求**返回之后**才发，而轮次前缀必须赶在第一个 token **之前**打出来。消费方按「turn 变了就补前缀」惰性处理，就不会给没有 content 的轮次（模型直接发 tool_calls）留下悬空前缀。收尾轮的 token 标 `max_turns + 1`——那是它真实的请求序号，而 `Outcome.turns` 报的是 `max_turns`，两者差 1 是已知的计数口径问题。

**流式与「区分思考/答案」不可兼得，这是物理限制而非实现缺陷。** token 到达时，这一轮会不会有 `tool_calls` **还不知道**——`Thought` 与 `Answer` 的判据（`calls.is_empty()`）必须等请求结束才能求值。所以：

| 要什么 | 怎么用 | 代价 |
|---|---|---|
| 逐字输出 | `on_token` | 内容只能统一渲染，分不出思考与答案 |
| 分得清 | `on_step` | 内容整段到达，没有逐字效果 |

`ReactLoop::run` 同时接收 `on_step` 与 `on_token` 两个回调；`examples/react_chat.rs` 传入空的 `on_token`（只消费 `on_step`）——它要演示的是**循环结构**，而 `examples/stream_chat.rs` 已经覆盖了流式。想两者都要，得靠 ANSI 回写重打，属于渲染层的事，不该让 `Step` 去承担。

**流式下的 `tool_calls` 按 `index` 重组。** chunk 里的 `delta.tool_calls` 是分片到达的——`id` 和 `name` 通常只出现在第一片，`arguments` 被切成多片。`src/llm/models.rs` 的 `ToolCallAccumulator` 负责拼接：按 `index` 找槽位（必要时 `resize_with` 补齐），逐片 `push_str`，`finish()` 时丢掉从没拿到 `name` 的空槽。

**改这段逻辑时注意**：`arguments` 是**字符串拼接**，不是 JSON 合并——中间态必然是非法 JSON，不能边收边解析。这条路径由 3 个单元测试覆盖（见 `commands.md` 的测试一节）。

## 6. provider 与模型 ID 必须配套

两者都来自环境变量，但由 `src/llm/provider.rs` 分别读取，行为不同：

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

## 9. RAG 检索层（内存版）

`src/rag/` 把检索拆成三个组件：`Embedder`（文本 → `Vec<f32>`）、`InMemoryStore`（内存向量库）、`Retriever`（组装层）。完整说明与流程图见 `src/rag/README.md`；这里只列跨组件的陷阱：

- **入库与查询必须同一 embedding 模型。** 更换 `EMBEDDING_MODEL_ID` 而沿用旧数据 = 拿错尺子量：分数照算、零报错，只是全部无意义。换模型必须重建索引。
- **维度守卫在 `InMemoryStore::insert`**：首条入库定下 `dim`，此后逐条校验；`search` 同样校验查询向量。缺了它，`zip` 对不等长切片**静默截断**（同第 5 节「违反不报错」家族）。
- **余弦而非欧氏距离**：文本 embedding 按余弦训练。零范数返回 `0.0` 防 NaN 传播；排序用 `total_cmp`（NaN 会让 `partial_cmp` 返回 `None`）。
- **`cosine_similarity` 是私有纯函数**，唯一生产调用方是 `search`（维度已由 store 校验），外部需要再放开。
- **边界**：无切块、无持久化、未接入 ReAct；store 只收算好的向量（不碰网络），因此它的测试完全离线。

## 10. 消息发送前后的回调（`Callback`）

`src/llm/callback.rs` 的回调链直接挂在传输层的唯一类型 `LLMClient` 上（`LLMClient::with_callbacks`，派发在模块内的 `prepare` / `conclude`）：每轮请求前后派发一次，`ReactLoop`、`History`、`src/tools/` 的**生产代码零改动**——不注册回调时，全链路与没有这层时逐字节一致。

**接缝形状。** 根特征 `Callback` 只有一个方法 `call(event)`，挂点做成数据（`CallbackEvent` 枚举）：`BeforeSend { messages: &mut Vec<_> }` 与 `AfterSend { messages: &[..], reply: &mut Reply }`。能力约束做进类型——`BeforeSend` 只给可变消息，`AfterSend` 消息只读、只有回复可改。枚举标 `#[non_exhaustive]`：将来新增挂点（工具前后、Step 事件等）只加变体，trait / 派发逻辑 / 既有实现都不变；实现用 `let CallbackEvent::X { .. } = event else { return Ok(()) };` 放行模板即可对新增变体免疫（外部 crate 的 `match` 必须带通配臂）。

**洋葱顺序。** `BeforeSend` 正序、`AfterSend` 逆序（注册 `[Logger, Redactor]` 时，`Logger.AfterSend` 看到的是 `Redactor` 处理过的最终回复）。

**线上 ≠ 存档（最关键的语义）。** `BeforeSend` 改的是「寄出去的信」：每轮从当前 `History` 重新克隆、重新派发，改动**不落历史**——这正是「发出去的比存下来的少」的裁剪刚需。`AfterSend` 改的是「回信」：`Reply` 回到 `ReactLoop` 后会原样落历史并驱动后续（改掉的 `tool_calls` 会被执行），因此它是**持久**的。想持久注入（如 RAG 片段要留给后续轮次）就不该用回调，那属于 `History` 的职责。

**fail-closed。** 任一回调返回 `Err`，整个请求失败并向上传播（`BeforeSend` 报错时内层传输层**不会**被调用）——与审批闸门「要问但无人可问 → 拒绝」同一姿态。想「尽力而为」的实现应自己吞错（`tracing::warn!` 后返回 `Ok(())`）。流式下 `AfterSend` 报错时 token 可能已经打出去，无法回滚。

**与四个不变量的关系。** 变异发生在 `ReactLoop` 看到 `Reply` **之前**，配对消息与终止判定都在最终值上计算；`AfterSend` 若删光 `tool_calls` 且 `content` 为空，会自然落入既有的 `Termination::EmptyReply`，不需要新分支。

**前缀缓存约束（`BeforeSend` 的核心）。** 主流 provider 对 prompt 的**最长公共 token 前缀**做 KV 缓存，从第一个 token 起精确匹配：注入要**拼在尾部**（插开头 / 中间会让插入点之后全部 miss）；裁剪 / 掩蔽要**攒批 + 滞回**（超上限才裁、一次裁到下限），两次事件之间保持 append-only，否则每轮前缀都在变、缓存全失效。用响应 usage 的 `prompt_tokens` / `prompt_cache_hit_tokens` 观测命中率。

**接缝分工。** `on_token`（逐 token 观察）、`on_step`（循环事件观察）、`Confirmer`（工具执行前批准 / 拒绝）都是「观察者」或「闸门」；`Callback` 是第一个**可变异**的接缝，所以是 async trait + `Result`。v1 不覆盖 GAIA 直答模式（它不走 `LLMClient`），也看不到轮次 / 阶段 / tools / `tool_choice`（留 v2）。参考实现见 `examples/callback_react.rs`（观察掩蔽 + 滑动窗口裁剪 + 动态注入 + 回复脱敏，离线可跑）。回调做持久化裁剪 / 摘要是**另一个机制**（管「存下来多少」），与本接缝（管「发出去多少」）互补。

## 11. Session 机制（多轮会话 / 多会话管理 / 审批挂起）

`src/session/` 与 `src/runtime.rs` 把「一次 run」升级成「一段可管理的会话」。组件关系：

```
Agent（runtime.rs：组装配置 + 委派）
  └── SessionManager（session/manager.rs：会话注册表）
        └── 每个 session 一个长期存活的 ReactLoop（多轮）
```

- **`Session`**（`session/models.rs`）就是你定的 schema：`session_id` / `user_id` / `history` / `state` / `created_at` / `updated_at`。**不额外存状态字段**：标题、消息数、挂起态全部从 `history` 派生（`title()` / `summary()` / `pending_call()`），schema 因此保持不动。`session/history` 的类型是 `Vec<ChatCompletionRequestMessage>`。
- **历史即检查点。** 挂起时不写任何额外数据——`run_pending_calls` 遇到 `Decision::Pending`（或无人可答）就原地返回，历史停在「assistant(tool_calls) + 前 k 条配对 tool 消息」上。恢复游标由纯函数 `history::pending_batch()` 反推（数最后一条带 `tool_calls` 的 assistant 后面跟了几条 tool 消息）。正常结束的 run 绝不留下未配对调用（不变量②），所以「有未配对调用 ⇔ 挂起」是充要条件。
- **`turn` 必须落在 `state` 里**：历史里没有轮次号，而恢复位置与 `max_turns` 预算都靠它。保留键 `turn` / `pause` 由 manager 维护，`pause` 只是展示副本。挂起**无限期**：没有超时、不自动批准/拒绝，只有显式 `resume` 才推进；`SessionSummary` 的 `suspended` / `pending_tool` 让它可见，`resume` 按 id 寻址所以随时可以回来批。
- **loop 是活体、`history` 是快照。** `SessionEntry { session, engine }` 把数据与执行者放在一起；每次 `send` / `resume` 收尾（含挂起）把 `engine.history()` 回写 `session.history`、刷新 `updated_at`。v2 文件后端冷启动时用 `ReactLoop::from_history()` 反向重建——**快照就是恢复源**。
- **并发**：外层 `std::sync::RwLock` 只做查表（临界区**绝不跨 await**），每个会话一把 `tokio::sync::Mutex` 在整轮 run 期间持有 → 同一 session 串行排队、不同 session 互不阻塞。写回因此不需要 CAS。
- **`create` 时即写入 system prompt**，所以不存在「空历史」的会话；之后一律以历史里的 system 为准（历史即事实），换 prompt 不会追溯升级既有会话。
- **本轮边界**：内存实现，退出即丢；跨重启保留要等文件后端（那时更可能把持久化拆成 `SessionStore` 挂在 manager 内部，而不是写一个把 loop 管理复制一遍的 `FileSessionManager`）。
- **交互式循环住在 `Agent::run`**：读入 →（斜杠命令 | 追问）→ 驱动一轮 → 展示，直到输入结束或 `/quit`；当前会话（首次追问自动新建、`/new` `/switch` `/delete` 改它）由循环自己维护。I/O 通过 `Console` trait 挡在库外（`read_line` / `print` / `step`），`Agent` 因此仍是无隐式 IO 的库组件——示例接 stdin，测试接脚本化输入。单轮出错（网络、挂起态被追问……）只打印一行 `[错误]` 并继续循环；流式 token 暂不投递（`step` 已交付完整答案，同时投递会打印两遍）。

**HRTB 陷阱（改 `ReactLoop` 回调签名前必读）。** `ReactLoop::run` / `resume` 的回调参数**只能**是 `&mut (dyn for<'x> FnMut(&'x Step) + Send)`——不能是泛型 `impl FnMut(&Step)`。原因：session 层持有的是 trait object，而 `impl FnMut(&Step)` 与 `&mut dyn FnMut(&Step)` 之间隔着 `impl FnMut for &mut F` 这条泛型实现，编译器无法为它推出 `for<'a>` 绑定（报 `FnMut is not general enough` / `borrowed data escapes outside of closure`），连用闭包手工适配 `|step| on_step(step)` 也一样。所以只有一套入口，调用方传闭包时写 `&mut |..|`（回调必须 `Send`）。另外 `async_trait` 展开后会丢掉高阶绑定，`for<'x>` 必须显式写。

**影响面**：老路径（`ReactLoop::new` + `run`）行为不变，GAIA / `main.rs` / `mcp_chat` 逻辑不受影响；但 `run` / `resume` 的签名变了——所有调用点写 `&mut |..|`，且 `on_step` 要求 `Send`（原先不要求，测试里拿 `RefCell` 收集 `Step` 的写法已改 `Mutex`）。

## 12. 工具 schema 归一化（`utils::schema`）

`Tool::definition()` 是工具定义发往服务端的**唯一出口**，它会对 `parameters()` 做一次
`utils::schema::flatten_tagged_union`，因为 `schemars` 对「内部标签枚举」只会生成顶层 `oneOf`：

```rust
#[serde(tag = "command", rename_all = "snake_case")]
pub enum EditFileArgs { StrReplace { .. }, Insert { .. }, ReplaceAnchor { .. } }
// schema_for! 得到：{ "oneOf": [ {..}, {..}, {..} ], "title": "EditFileArgs" }
```

而 OpenAI 要求 function parameters 顶层是 `type: "object"`，否则整个请求 400（不是这一个工具被跳过，是**整轮**失败）：

```text
400 Bad Request invalid_request_error: Invalid schema for function 'edit_file':
schema must be a JSON Schema of 'type: "object"', got 'type: null'.
```

压平规则（`src/utils/schema.rs` 的 doc 有完整版）：`properties` 取并集；同名属性两边都是 `const`/`enum` 时合并成取值枚举；`required` 取**交集**（只在部分分支必填的字段降级为可选，缺字段由运行期反序列化报错、再压成 Observation 让模型自纠）。**保守**：仅当每个分支都是带 `properties` 的 object 时才动手，否则原样放行。

同一个出口还会收敛 `Option<T>` 带来的 `"type": ["X", "null"]`（`schemars` 的产物）：这些字段本来就不在 `required` 里，模型完全可以省略，而 OpenAI 对 union type 支持不稳。收敛不改变运行期行为——`Option<T>` 对「缺字段」与「显式 null」都能反序列化；只有「去掉 `null` 后恰好剩一个类型」时才改，多类型联合原样保留。

这是编译期宏产物，本地测试看不见——`tools::tests::every_tool_definition_is_a_top_level_object_schema` 钉住了实际发出去的形状。新增工具时不要绕过 `definition()` 直接拼 `FunctionObject`；MCP 工具的服务端 schema 也走同一条路。
