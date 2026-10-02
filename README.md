# coding_agent

一个 Rust 编写的编码 Agent，包含带工具调用的 ReAct 循环，并支持通过 **MCP（Model Context Protocol）** 接入外部工具。

## 目录结构

```
src/
├── agent/
│   ├── llm/          # LLM 客户端、provider 配置、并发信号量
│   ├── rag/          # RAG 检索：embed / store / retriever（说明见 rag/README.md）
│   └── react/        # ReAct 循环（runner/history/models）
├── tools/
│   ├── tool.rs       # Tool trait
│   ├── local/        # 本地（进程内）工具，每个工具一个子目录
│   │   ├── web_search/
│   │   └── final_answer/
│   └── mcp/          # MCP 支持（远端工具）
│       ├── config.rs     # mcp.json 解析与校验
│       ├── connection.rs # 启动子进程、握手、工具发现
│       └── tool.rs       # 远端工具 -> 本地 Tool 适配
├── gaia/             # GAIA 数据集评测
└── constant/         # 常量与 prompt
examples/             # 可运行示例
tests/fixtures/       # 测试用假 MCP server（Python）
```

## MCP 支持范围

- **传输**：仅 **stdio**（由 agent 启动本地子进程，通过 stdin/stdout 走换行分隔的 JSON-RPC 2.0）。
- **能力**：仅 **tools**（`initialize` / `tools/list` / `tools/call`）。
- **角色**：本项目是 MCP **Host**；每个 server 对应一个 MCP **Client**（1:1）；被启动的子进程是 MCP **Server**。
- **工具命名**：暴露给模型的名字为 `{server}__{tool}`，且必须满足 `^[a-zA-Z0-9_-]{1,64}$`，否则该工具会被跳过并告警。
- **注册时机**：启动时发现一次并快照。

**不在范围内**：Streamable HTTP / SSE、`resources`、`prompts`、`sampling`、`elicitation`、`roots`、`subscriptions`/`progress`、运行时工具列表刷新（`list_changed`）、断线重连、认证。

## 配置 `mcp.json`

只读取**当前工作目录**下的 `mcp.json`（当前不支持用环境变量覆盖路径）。

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": { "TOKEN": "xxx" },
      "cwd": "/optional",
      "required": false,
      "timeoutSecs": 60
    }
  }
}
```

| 字段 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `command` | 是 | — | 可执行文件（如 `npx`、`uvx` 或绝对路径） |
| `args` | 否 | `[]` | 启动参数 |
| `env` | 否 | `{}` | 追加/覆盖到继承的父进程环境 |
| `cwd` | 否 | 父进程 CWD | 子进程工作目录 |
| `required` | 否 | `false` | 为 `true` 时该 server 启动失败会让整体启动失败 |
| `timeoutSecs` | 否 | `60` | 每次 `tools/call` 的超时秒数 |

server 名（即 `mcpServers` 的键）只能包含字母、数字、下划线和连字符。

### 行为与错误语义

| 情况 | 行为 |
|---|---|
| `mcp.json` 不存在 | 只注册内置工具，不报错 |
| `mcp.json` 非法 / 校验失败 | 启动返回错误（fail fast） |
| 某 server 连接失败，`required=false` | 打告警并跳过 |
| 某 server 连接失败，`required=true` | 启动返回错误 |
| 连接成功但没有工具 | 打告警并丢弃该连接 |
| 工具名与已注册工具冲突 | 先到先得（本地优先），后者跳过并告警 |
| 工具调用超时 / 协议错误 | 返回错误观察，不中断 ReAct 循环 |
| 工具级错误（`isError=true`） | 作为正常观察文本回给模型，让其自行纠错 |

日志统一使用 `target = "mcp"`，便于过滤。

## ReAct 循环：强制工具调用与 `final_answer`

循环内每一轮都以 `tool_choice=required` 发请求，并显式打开 `parallel_tool_calls`。后者只是**请求**而不是契约：端点可以忽略它（DeepSeek 的参数表里没有这个字段），所以一轮多个 `tool_call` 会被当作正常输入处理。

最终答案被抽象成一个普通工具 `final_answer`（注册在本地工具表里）。它是「交付」而不是真正的工具：参数即答案，`extract_answer` 输入即输出，**不执行、不发 Observation**。模型**只能**通过调用它来结束任务；在 `required` 语义下 `content` 只是思考。

| 情况 | 行为 |
|---|---|
| 模型调用 `final_answer`（参数合法） | 立即交付（`Termination::FinalAnswer`）。同轮其它调用**一律不执行**，但都会补一条配对 tool 消息（被跳过的回填占位文本），历史因此始终可重放 |
| `final_answer` 参数非法 | 不交付：压成 Observation 让模型重试；同轮其它调用照常执行并配对 |
| 一轮里 `final_answer` 与其它工具并存 | 交付优先且顺序无关（`find_final_answer`），其余调用不执行 |
| 非 function 类型的调用（如 `Custom`） | 无法执行也无法回填，落库前丢弃并告警 |
| 撞轮次上限 | 收尾轮先把工具面裁到只剩 `final_answer`、追加一条 system 指令，再用 `tool_choice` 具名强制它，并发出 `Step::Answer`（不执行、不发 Action/Observation）。拿不到合法参数时退回 `content`，仍为空则交付固定兜底文案，**不报错** |
| 端点无视 `required`、只回纯文本 | 打告警并按纯文本答案收尾，不中断 |
| 端点 400 拒绝 `tool_choice` | 打告警，后续请求降级为 `auto` 重发一次 |
| 未声明任何工具（如 `stream_chat`） | 请求体不带 `tool_choice` |

## RAG 检索（内存版）

`src/agent/rag/` 打通「文本入库 → 向量检索」链路：`Embedder`（文本 → 向量）、`InMemoryStore`（内存向量库，全扫余弦取 top-k）、`Retriever`（组装层）。embedding 端点独立配置（`EMBEDDING_*` 环境变量），与 LLM provider 解耦。

组件说明与交互流程图见 [`src/agent/rag/README.md`](src/agent/rag/README.md)。当前边界：无切块、无持久化、未接入 ReAct。

## 运行

```bash
# 准备配置（mcp.json 已被 .gitignore 忽略）
cp mcp.example.json mcp.json

# 常规构建与测试
cargo build --all-targets
cargo test --all-targets

# MCP 集成测试（需要本机 python3，默认 #[ignore]）
cargo test --lib -- --ignored

# 手动验证：连接一个 stdio MCP server 并调用一次 echo
cargo run --example mcp_probe
cargo run --example mcp_probe -- npx -y @modelcontextprotocol/server-everything

# 端到端：用户提问 → ReAct 循环 → 调用 MCP 工具（脚本化模型，无需 LLM 凭证）
cargo run --example mcp_react
cargo run --example mcp_react -- npx -y @modelcontextprotocol/server-everything

# ReAct 对话示例
cargo run --example react_chat -- "什么是 MCP?"

# RAG 端到端检索：ingest 若干文本 → 提问 → 打印 top-k（会真实调用 embedding 端点，需要 EMBEDDING_*）
cargo run --example rag_chat
cargo run --example rag_chat -- "余弦相似度怎么算？"

# 真实 LLM + MCP：先准备好 mcp.json 与 .env（会真实调用 LLM）
cargo run --example mcp_chat
cargo run --example mcp_chat -- "用 MCP 的 echo 工具确认链路"

# 查看当前注册的工具表
cargo run --example tool_exec -- "rust async"

# GAIA Level 1 对比评测：每题各跑一次「带工具 / 不带工具」，输出两组通过数/通过率
# 需要 HF_TOKEN；带工具组使用本地工具 + mcp.json 中的 MCP 工具
cargo run --bin gaia
```

## 已知限制

- **进程异常退出探测**：子进程非正常退出只能在**下一次调用**时报错暴露，没有主动 watcher。
- **启动快照**：工具列表在启动时确定，运行期不刷新，也不重连。
- **仅 stdio**：不支持远程 HTTP MCP server。
- 不支持需要服务端反向请求的能力（sampling / elicitation / roots），遇到 `InputRequired` 或 `Task` 响应会直接报错。
