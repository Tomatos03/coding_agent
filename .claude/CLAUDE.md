# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目定位

从零手写的 coding agent（Rust 2024 edition，crate 名 `coding_agent`）。目标形态是「模型请求 → 工具调用 → 结果回填」的自主循环。

**ReAct 主循环已落地**：`src/react/runner.rs` 的 `ReactLoop`——思考 → 行动 → 观察。循环内每轮 `tool_choice=required`，最终答案被抽象成 `final_answer` 工具（`execute` 输入即输出），因此模型只能通过调用它来结束；撞轮次上限则用 `tool_choice` 具名强制 `final_answer` 收尾，而不是报错退出。

**MCP 工具支持已落地**：`src/tools/mcp/` 通过 **stdio** 启动本地 MCP server、发现工具并适配成本地 `Tool`（一期仅 `tools`、仅 stdio）。本地（进程内）工具放在 `src/tools/local/`，每个工具一个子目录。

**RAG 检索切片已落地**：`src/rag/` 的 `Embedder`（文本 → 向量，走独立的 `EMBEDDING_*` 配置）+ `InMemoryStore`（内存向量库，全扫余弦取 top-k）+ `Retriever`（组装层）打通「入库 → 检索」链路；模块说明与交互流程图见 `src/rag/README.md`。尚未做的：长文切块、持久化、接入 ReAct。

仍缺的是**对话历史的截断/摘要策略**——`History` 只增不减，长会话迟早撑爆 context。流式的工具调用已经实现（`ToolCallAccumulator` 按 `index` 重组分片），所以 `ReactLoop` 的流式与非流式两条路都能调工具。

## 分层与依赖方向

```
src/llm/     传输层：一段消息进、一条回复出。无状态
src/react/   编排层：ReAct 循环、消息历史
src/tools/         工具层：spec / execute
  ├── local/       本地（进程内）工具，每个工具一个子目录
  └── mcp/         MCP 远端工具（stdio）
```

依赖是**单向**的：`react → llm`。判据有两条，违反时编译器不会提醒：

- `src/llm/` 里**不应出现 `use crate::tools::...`**——传输层只收调用方递进来的 `Option<&[ChatCompletionTools]>`，不认识 `ToolHashMap`。
- `src/llm/` 里不应出现具体领域类型（工具参数、编排层的词汇类型）。

编排层只依赖 `llm::models::LLMClient` 这**一个具体类型**（没有 trait，也没有 `LLMClient` / `CallbackCompleter` 之类的实现类型）：它内部装配私有 `Live`（真实网络）或脚本化后端，所以 `ReactLoop` 的全部逻辑可以离线测试。

## 模块职责

| 模块 | 职责 |
|---|---|
| `src/llm/models.rs` | `LLMClient`：**唯一**的传输层类型（无 trait）。内部后端二选一——私有 `Live`（model 名 + `async_openai::Client`，端点以 400 拒绝 `tool_choice` 时用粘性 `AtomicBool` 标记并降级为 `Auto` 重发一次）或测试 / 离线示例的脚本化后端。`complete`（一次性）与 `stream`（逐 token 回调）都接收 `ToolPolicy`（`Auto` / `Required` / `Force(工具名)`，请求级 `tool_choice`）并返回领域类型 `Reply`（`content` + `tool_calls`）；请求前后的回调链也挂在这一层（`with_callbacks`，派发见 `src/llm/callback.rs`） |
| `src/llm/test_support.rs` | 脚本化后端与请求快照：`LLMClient::scripted(replies)` 挂载，逐次记录 messages / 工具名 / `ToolPolicy`；单测与离线示例据此离线驱动并断言整条链路 |
| `src/llm/provider.rs` | provider 选择、`client_config()`、模型 ID 读取、schema 降级提示 |
| `src/llm/semaphore.rs` | 进程级并发闸门（3 permits），**由调用方负责获取** |
| `src/react/runner.rs` | `ReactLoop`：循环推进、工具派发、终止判定。`run` 同时接收 `on_step` 与 `on_token` 两个回调（原 `run_stream` 已并入 `run`） |
| `src/react/history.rs` | `History`：消息序列的薄封装（`system`/`user`/`assistant`/`tool` + `as_slice`） |
| `src/react/models.rs` | 编排层词汇：`Step` / `Termination` / `Outcome` / `DEFAULT_MAX_TURNS` |
| `src/rag/embed.rs` | `Embedder`：文本 → `Vec<f32>`。端点与模型经 `provider::embedding_client_config()` / `embedding_model_id()` 从 `EMBEDDING_*` 读取，与 provider 选择解耦 |
| `src/rag/store.rs` | `InMemoryStore`：`Chunk { vector, text }` 入库（维度守卫）+ `search` 全扫余弦、降序取 top-k，返回 `SearchHit { score, text }`；`cosine_similarity` 是私有纯函数 |
| `src/rag/retriever.rs` | `Retriever` 组装层：`ingest(text)` = embed + insert，`retrieve(query, top_k)` = embed + search |
| `src/tools/tool.rs` | `Tool` trait：`name` / `description` / `parameters` / `execute`；`definition()` 是默认实现，产出 wire format |
| `src/tools/local/web_search/mod.rs` | web_search 工具：参数/响应类型与 `Tool` 实现合并在一个文件 |
| `src/tools/local/final_answer/mod.rs` | final_answer 工具：最终答案的收尾通道。`execute` **输入即输出**（原样返回 `answer` 参数），`extract_answer` 供循环做终止判定 |
| `src/tools/mcp/config.rs` | 读取并校验 `mcp.json`（`McpConfig` / `McpServerConfig`） |
| `src/tools/mcp/connection.rs` | `McpConnection`、`connect` / `connect_all`：启动子进程、握手、`tools/list`、失败隔离与子进程 keep-alive |
| `src/tools/mcp/tool.rs` | `McpTool`：远端工具 -> 本地 `Tool` 适配（命名、调用、`CallToolResult` 映射） |
| `src/tools/mod.rs` | `ToolHashMap`（`HashMap<String, Arc<dyn Tool>>`，值用 `Arc` 所以整张表可廉价克隆）与 `build_tools()`（异步、合并本地 + MCP）/ `build_tools_with(config)`（不读文件的测试接缝） |
| `src/gaia/` | 独立的评测垂直切片：HF 拉数据集 → 每题分别按「直答」与「ReAct + 工具」两种模式求解 → 比对答案（`report.rs` 按模型×模式汇总通过率）。除借用 `llm::provider` 的客户端配置外，带工具模式还向下依赖 `react`（`ReactLoop`）与 `tools` |
| `src/constant/` | 按领域分的字面量常量：`provider.rs`（provider 名与凭证环境变量名）、`prompt.rs`（`SYSTEM_PROMPT`）、`gaia.rs`（评测参数）、`embedding.rs`（embedding 端点与模型的环境变量名）。模型 ID 本身走 `CURRENT_USE_MODEL_ID`，不硬编码 |
| `src/bootstrap.rs` | dotenv + tracing 的统一初始化入口 |

可执行入口：`src/main.rs`（交互式 Agent REPL：接线 stdin/stdout + 交互确认方后跑 `Agent::run`）、`src/bin/gaia.rs`（GAIA 对比评测：每题各跑一次「带工具 / 不带工具」，输出两组通过数与通过率）。`examples/` 下九个：

| 示例 | 演示什么 |
|---|---|
| `react_chat` | ReAct 主循环，逐轮打印 `Thought` / `Answer` / `Action` / `Observation`（**非流式**，因为流式下无法在打印时区分思考与答案） |
| `stream_chat` | 流式输出 |
| `semaphore_chat` | 并发限流：5 个任务抢 3 个 permit |
| `web_search` | 裸 HTTP 打 Tavily + 响应解析 |
| `tool_exec` | 不走 LLM，直接验证「注册表 → trait 对象 → execute」 |
| `mcp_probe` | 连接一个 stdio MCP server，打印/调用适配出的工具（默认用 `tests/fixtures/fake_mcp_server.py`） |
| `mcp_react` | 端到端：用户提问 → ReAct 循环 → 调用 MCP 工具 → 汇总回答（脚本化 `LLMClient`，无需 LLM 凭证） |
| `mcp_chat` | 真实 LLM + MCP：从 `mcp.json` 加载工具，模型自主决定是否调用（需要凭证与 `mcp.json`） |
| `rag_chat` | 端到端检索：ingest 若干文本 → 提问 → 打印 top-k 与 score（需 `EMBEDDING_*` 凭证） |

## 深入阅读

- @rules/architecture.md — 跨文件的架构约束与已知陷阱（**改 LLM 调用、并发或重试前必读**）
- @rules/commands.md — 构建、运行、环境变量
- @rules/rust-conventions.md — 代码级约定（处理 `**/*.rs` 时自动加载）
