//! REPL 组件：写死「读取 → 评估 → 输出」的循环，三段各是一个扩展点。
//!
//! [`Repl`] 从 [`Reader`] 拉一条输入，交给 [`Evaluator`] 评估，再把评估产出逐条
//! 交给 [`Writer`] 展示。三段之间不做类型预设：`Reader::Out` 进 `Evaluator`、
//! `Evaluator::Out` 进 `Writer`，全部由各实现自己决定（关联类型接线）。I/O 与
//! 交互策略因此都挡在库外；斜杠命令等协议在 [`AgentEvaluator`] 里，不属于框架。
//! 库另带现成的终端适配（[`StdinReader`] / [`StdoutWriter`]），入口与示例直接复用。

pub mod agent;
pub mod terminal;

use async_trait::async_trait;

pub use agent::{AgentEvaluator, Output};
pub use terminal::{StdinReader, StdoutWriter};

/// 评估的单条产出：一条要展示的内容（交由 [`Writer`]），或终止循环的请求。
#[derive(Debug)]
pub enum Emit<T> {
    /// 交给 [`Writer`] 展示的一条内容，循环继续。
    Continue(T),
    /// 请求退出循环；[`Repl`] 截获这一条，不会交给 [`Writer`]。
    Quit,
}

/// 输入源：`None` 表示输入结束（EOF / 通道关闭 / 脚本耗尽），不是错误。
///
/// `Out` 是本实现产出的输入类型（如 `String`）；[`Repl`] 原样交给 [`Evaluator`]，
/// 框架不认识具体类型。
#[async_trait]
pub trait Reader: Send {
    /// 本 Reader 产出的输入类型。
    type Out: Send;

    async fn read(&mut self) -> Option<Self::Out>;
}

/// 评估：把一条输入变成要展示的产出（可为多条）。
///
/// `In` 是 [`Reader::Out`]；`Out` 是本实现交给 [`Writer`] 的展示类型。
/// 单轮错误属于产出（打印一行 `[错误]` 即可）；返回 `Err` 视为致命，[`Repl::run`]
/// 会终止。需要反向读入的审批问答走独立的确认接缝（`react::approval::Confirmer`），
/// 不塞进这里。
#[async_trait]
pub trait Evaluator<In: Send>: Send {
    /// 本 Evaluator 交给 [`Writer`] 的展示类型。
    type Out: Send;

    async fn eval(&mut self, input: In) -> anyhow::Result<Vec<Emit<Self::Out>>>;
}

/// 展示：逐条消费 [`Emit::Continue`] 携带的内容（[`Emit::Quit`] 不会到达这里）。
pub trait Writer<In>: Send {
    fn write(&mut self, input: In);
}

/// 写死「读取 → 评估 → 输出」的循环组件：三段经关联类型接线，运行时零开销。
pub struct Repl<R, E, W> {
    reader: R,
    evaluator: E,
    writer: W,
}

impl<R, E, W> Repl<R, E, W> {
    pub fn new(reader: R, evaluator: E, writer: W) -> Self {
        Self {
            reader,
            evaluator,
            writer,
        }
    }

    /// 拆回三段，便于测试或复用。
    pub fn into_parts(self) -> (R, E, W) {
        (self.reader, self.evaluator, self.writer)
    }
}

impl<R, E, W> Repl<R, E, W>
where
    R: Reader,
    E: Evaluator<R::Out>,
    W: Writer<E::Out>,
{
    /// 循环本体：读一条 → 评估 → 逐条输出；EOF 或 [`Emit::Quit`] 终止。
    pub async fn run(&mut self) -> anyhow::Result<()> {
        while let Some(input) = self.reader.read().await {
            for emitted in self.evaluator.eval(input).await? {
                match emitted {
                    Emit::Continue(output) => self.writer.write(output),
                    Emit::Quit => return Ok(()),
                }
            }
        }
        Ok(())
    }
}
