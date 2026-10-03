# coding_agent

一个 Rust 编写的编码 Agent，包含带工具调用的 ReAct 循环，并支持通过 **MCP（Model Context Protocol）** 接入外部工具。

## 目录结构

```
src/
├── agent/
│   ├── llm/          # LLM 客户端、provider 配置、并发信号量
│   ├── rag/          # RAG 检索：embed / store / retriever（说明见 rag/README.md）
│   └── react/        # ReAct 循环（runner/history/models/approval）
├── tools/
│   ├── tool.rs       # Tool trait
│   ├── local/        # 本地（进程内）工具，每个工具一个子目录
│   │   ├── web_search/
│   │   └── final_answer/
│   └── mcp/          # MCP 支持（远端工具）
│       ├── config.rs     # mcp.json 解析与校验
│       ├── connection.rs # 启动子进程、握手、工具发现
│       └── tool.rs       # 远端工具 -> 本地 Tool 适配
├── settings.rs       # 危险工具审批策略（.agents/settings.json 的解析与匹配）
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

## 本地文件工具

除 `web_search` / `final_answer` 外，本地工具表默认还注册 5 个文件工具。它们能访问哪些
路径由**文件权限**控制（`src/tools/local/permission.rs` 的 `Permission`），当前固定为
`workspace` 模式：只能访问**工作区根目录**（固定为进程当前目录，即 agent 的运行目录）
内的路径，`..`、根外绝对路径、指向根外的符号链接一律拒绝。无边界模式
`Permission::Full` 保留在类型里，但暂时写死不可选。

| 工具 | 参数 | 行为 |
|---|---|---|
| `list_files` | `path`（默认 `.`）、`recursive`（默认 `false`） | 按名字升序列出条目，目录以 `/` 结尾；上限 500 条，超出截断并提示 |
| `read_file` | `path`、`offset`、`limit`、`line_anchors` | 返回带行号的 UTF-8 文本；`line_anchors=true` 时每行改成 `ANCHOR│内容`，并把展示过的行登记为服务记录；默认最多 2000 行、上限 5000 行，输出上限 256 KiB；非 UTF-8 / 含 NUL 的二进制文件报错 |
| `write_file` | `path`、`content` | 整文件新建或覆盖，自动创建缺失父目录 |
| `edit_file` | `command`、`path`、各命令参数 | `command=str_replace` 精确字符串替换（默认要求唯一，`replace_all` 替换全部）；`command=insert` 在第 N 行后插入；`command=replace_anchor` 用行锚点整段替换，并按行指纹做冲突检测。临时文件 + rename 原子写回 |
| `delete_files` | `paths`（非空字符串数组） | 批量删除文件或目录（目录**递归删除**）；先整体校验再删；不跟随最后一段符号链接（删链接本身） |

`read_file` 传 `line_anchors=true` 时每行输出 `ANCHOR│内容`：锚点由本地工具内的
**分配式账本**（`src/tools/local/anchor_registry.rs`）分配，先按归一化行内容的
64 位哈希（`src/utils/hash.rs`）选槽位，冲突时用固定步长线性探测（`src/utils/anchor.rs`）。
锚点在同一文件内唯一，内容相同的重复行也会拿到不同锚点；一次编辑之后，范围外、
内容未变的行锚点保持不变。

`read_file` 会把展示过的行登记为**服务记录**（锚点 → 完整行指纹）。`edit_file` 的
`command=replace_anchor` 用 `remove_from` / `remove_to` 锚点整段替换（空
`replacement_lines` 即删除），编辑前对范围内每一行比对行指纹：内容变了的行会被拒绝
（`[E_RANGE_STALE]`）并回传当前范围与新锚点，重试无需重新读取。`str_replace` 仍以
唯一字符串匹配保证正确性。哈希、归一化与锚点编解码等纯函数都在 `src/utils/` 下。

路径权限集中实现在 `src/tools/local/permission.rs` 的 `Permission`：按模式调度
（`workspace` 模式的实际边界逻辑在 `src/tools/local/workspace.rs` 的 `Workspace`）。
工具执行失败会返回 `Err`，由 ReAct 循环压成 Observation 让模型自行纠错，不中断循环。

文件工具各有一个示例（不经过 LLM）。除 `read_file` 读取命令行指定的文件外，其余示例都会在临时工作区里造数据、打印结果、最后清理：

```bash
cargo run --example list_files
cargo run --example read_file -- src/lib.rs --limit 5
cargo run --example write_file
cargo run --example edit_file
cargo run --example delete_files
```

## 危险工具确认（`.agents/settings.json`）

ReAct 循环执行任何工具前会查一次审批策略：判 `ask` 的调用先经确认方征求同意，
**拒绝被压成一条 Observation**——不执行、不中断循环，模型可按提示改用其它方案。
工具层与传输层不感知这件事，判定完全由配置驱动（同一工具在交互终端与评测批处理里
可以有不同待遇，改配置不需要改代码）。

配置固定在**工作区根目录**（即进程当前目录）下的 `.agents/settings.json`（已 gitignore）：
读取不到时使用默认配置（**全放行**，与没有这道闸门时行为一致）。仓库提交
`.agents/settings.example.json` 作推荐配置：

```json
{
  "approval": {
    "defaultAction": "allow",
    "rules": [
      { "pattern": "*__*", "action": "ask" },
      { "pattern": "write_file", "action": "ask" }
    ]
  }
}
```

| 字段 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `defaultAction` | 否 | `"allow"` | 没有任何规则命中时的动作 |
| `rules[].pattern` | 是 | — | 匹配工具暴露名（`filesystem__write_file`、`web_search`）；glob 风格，`*` 通配任意字符序列（含空）；空串报错 |
| `rules[].action` | 是 | — | `"allow"` / `"ask"` |

规则自上而下、**第一条命中即生效**；全名锚定、大小写敏感。`*__*` 恰好命中一切
`{server}__{tool}` 形式的 MCP 工具。字段名拼错在启动时立即报错（`deny_unknown_fields`）。

| 情况 | 行为 |
|---|---|
| `.agents/settings.json` 不存在 | 全放行，不报错 |
| 策略判 `ask`、调用方注入了确认器 | 先征求同意再执行；被拒绝 → Observation，循环继续 |
| 策略判 `ask`、但没有确认器（fail-closed） | 直接拒绝执行，同样压成 Observation |
| `final_answer` 与撞上限的收尾轮 | 不执行工具，天然豁免闸门 |

`examples/mcp_chat` 内置终端交互确认（stdin `y`/`n`）；`examples/mcp_react` 用脚本化
确认方离线演示「先拒绝、后批准」，无需凭证。

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
# 准备配置（mcp.json 与 .agents/settings.json 已被 .gitignore 忽略）
cp mcp.example.json mcp.json
cp .agents/settings.example.json .agents/settings.json   # 可选：启用危险工具确认

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
