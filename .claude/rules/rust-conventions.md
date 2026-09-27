---
paths:
  - "**/*.rs"
---

# Rust 代码约定

## 模块布局

- **模块入口一律用 `mod.rs`**，不采用「子目录 + 父目录同名 `foo.rs`」的写法。即写 `src/foo/mod.rs` + `src/foo/bar.rs`，不写 `src/foo.rs` + `src/foo/`。
  crate 根是唯一例外——`src/lib.rs`、`src/main.rs`、`src/bin/*.rs` 的文件名由 Cargo 规定，不适用本规则。
  理由：`mod.rs` 在目录内直接标记模块入口，不必跨两个位置拼接才能看清模块边界。clippy 的 `self_named_module_files` 正是校验这条规则的 lint（实测：对自命名风格报 "`mod.rs` files are required"）。
  **注意不要开 `mod_module_files`**——它是反过来的那条，禁止 `mod.rs`。两者名字极易记混，实际语义以运行时行为为准。
- **按概念命名文件，不按语言构造命名**。避免 `models.rs` / `impls.rs` 这种「按种类分堆」的文件名，它们会随规模增长变成杂物间。文件应回答「这里面是什么概念」：例如 `src/tools/mcp/{config,connection,tool}.rs`、`src/tools/local/web_search/mod.rs`。
  **带行为的结构体与它的 `impl`（含 `Tool` 的 trait 实现）放在同一个文件**。拆开不仅多一次跳转，还会逼字段用 `pub(crate)` 开口子；样板是 `src/tools/mcp/tool.rs`（`McpTool` + `impl Tool`）。
  `models.rs` 仍适用于**纯领域词汇**（会流动的数据，如 `src/agent/react/models.rs` 的 `Step` / `Termination` / `Outcome`、`src/gaia/models.rs`）——它是「内容正好是词汇」的结果，不是「有目录就必须有」的规定。
- **本地工具与 MCP 工具分目录**：本地（进程内）工具放 `src/tools/local/<tool>/`，并在 `local/mod.rs` 里声明 `pub mod` 与 `pub use`；MCP 远端工具在 `src/tools/mcp/`。新增本地工具时不要再往 `src/tools/` 根下加目录。
- **模块必须被声明才会编译**。新建 `foo/bar.rs` 后忘了在 `foo/mod.rs` 里写 `pub mod bar;`，Rust **不报错也不警告**，那个文件被静默忽略，表现为「明明写了却找不到符号」。`constant/mod.rs` 里的 `pub mod` 与常量本身的 `pub` 同样缺一不可。
  同理，`#[cfg(test)] mod tests` 里的 `use super::*;` **只带得进父模块自己 import 过或定义过的东西**。类型被搬到别的文件后，测试要显式补 import——这个错会表现为「明明在同一个 crate 里却找不到」，而 IDE 的「优化 import」还可能误删那行。

## 错误处理

统一用 `anyhow`：函数签名写 `anyhow::Result<T>`，用 `?` 传播。构造临时错误用 `anyhow::anyhow!`，条件缺失走 `.ok_or_else(|| anyhow::anyhow!("..."))` 而非 `unwrap()`——现有代码在解析 LLM 响应时一律如此（`src/agent/llm/models.rs` 的 `Completer`、`src/agent/react/runner.rs`）。

**但有一处刻意例外**：`ReactLoop::execute` 的返回类型是 `String` 不是 `Result<String>`，因为工具失败必须变成一条 Observation 而不是中止循环。详见 `@rules/architecture.md` 第 5 节。

`bail!` 只在**错误由当前代码自己判定**、手边没有现成 `Result` 可传播时用（例如 `let ... else` 的 `else` 块，那里要求类型为 `!`）；下游已经返回了错误的场合一律用 `?`。

`anyhow::Ok` **不是** `std::result::Result::Ok` 的 re-export，而是 anyhow 定义的一个**函数**：

```rust
pub fn Ok<T>(value: T) -> Result<T>   // 等价于 Ok::<_, anyhow::Error>(value)
```

它的唯一用途是类型推断——`let x = Result::Ok(1)` 会因 `E` 推不出来报 E0282，`anyhow::Ok(1)` 则把 `E` 钉死成 `anyhow::Error`。函数签名已经写明错误类型时（如 `-> anyhow::Result<Vec<GaiaRow>>`）用不用都行，`src/gaia/dataset.rs` 用了，可直接沿用，不要「顺手清理」。

## 初始化与日志

- 入口函数第一件事是 `bootstrap::init()`，它已包含 `dotenv` 与 `tracing`。
- 日志用 `tracing` 宏；打印结构化响应时沿用量级较高的 `tracing::info!("LLM Response: {:#?}", response)` 形式，便于对拍。`print!` 仅用于流式输出的逐 token 呈现（`Completer::stream` 通过 `on_token` 回调把 token 交给调用方打印）。

## 常量与命名

- 环境变量名、provider 名、默认值这类**字面量常量**按领域放 `src/constant/<领域>.rs`（`provider.rs` / `prompt.rs` / `gaia.rs`），`SCREAMING_SNAKE_CASE`，类型显式写 `&'static str`。
- **模型 ID 不硬编码**：它由 `CURRENT_USE_MODEL_ID` 经 `provider::model_id()` 读取。原因是 ID 与 provider 强绑定且命名规则不同（OpenRouter 用 `<org>/<model>:<variant>`，DeepSeek 用裸名称），单个常量无法同时适配。`src/constant/provider.rs` 里只放该环境变量的**名字**（`MODEL_ID_ENV`）。
- 数值字面量带类型后缀（`2048u32`），与 `ChatCompletionRequestArgs` 的泛型参数配合。
- 占位实现的未用参数加 `_` 前缀（`_tool`），不要 `#[allow(unused)]`。

## 注释

默认**不写任何注释**——`//`、`///`、`//!` 都不写。需要注释时由使用者显式提出。

唯一的例外是那些**不只是注释**的文档注释：`schemars` 会把字段上的 `///` 编译进 JSON Schema 的 `description`，而那份 schema 是发给模型看的。这类信息改用属性表达，既保留运行时效果又不再是注释：

```rust
#[schemars(description = "待求值的表达式，例如 (1 + 2) * 3。")]
pub expression: String,
```

见 `src/tools/local/web_search/mod.rs` 的 `WebSearchArgs`——那份 schema 会随 `tools` 参数发给模型，所以字段说明是**运行时行为**的一部分，不是文档。

## 流式消费

用 `futures::StreamExt`，再 `while let Some(item) = stream.next().await`（`src/agent/llm/models.rs` 的 `Completer::stream` 就是）。

`async-openai` 的流式 chunk 里，token 在 `chunk.choices.first()?.delta.content`——用 let-chains 一层层剥：`if let Some(choice) = chunk.choices.first() && let Some(delta) = &choice.delta.content`。

`Completer::stream` 用 **`on_token` 回调**把 token 交给调用方打印，函数最后返回 `Reply`——不是返回一个 `Stream`。原因是「边吐 token 边用」和「交出一个 `Stream` 让调用方自己驱动」是两种消费姿势，回调版在「打印到终端」这个主场景下更好用。要真正的 `Stream`，用 `futures::stream` 或 `async_stream::stream!` 自己包一层——`Cargo.toml` 里 `async-stream` 与 `uuid` 已就位但目前没有用例。
