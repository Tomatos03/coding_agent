//! 终端适配：把 stdin/stdout 接成 [`Reader`] / [`Writer`]，入口与示例直接复用。
//!
//! 这些是显式构造的便捷实现，不引入隐式 I/O——库里其余部分仍然只依赖三个接缝。
//! 确认方（`TerminalConfirmer`）直接读 stdin，不经过 [`StdinReader`]。

use std::collections::VecDeque;
use std::io::Write;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

use super::{agent::Output, Reader, Writer};
use crate::react::models::Step;

/// 逐行读 stdin 的 [`Reader`]；用 [`with_pending`](StdinReader::with_pending)
/// 可以先喂入一批预置输入（如命令行上的首条提问），读完后再转向 stdin。
pub struct StdinReader {
    input: Lines<BufReader<Stdin>>,
    pending: VecDeque<String>,
}

impl StdinReader {
    /// 纯 stdin 输入。
    pub fn new() -> Self {
        Self::with_pending(Vec::new())
    }

    /// 预置一批输入：先按序吐出它们，耗尽后再读 stdin。
    pub fn with_pending(pending: Vec<String>) -> Self {
        Self {
            input: BufReader::new(tokio::io::stdin()).lines(),
            pending: pending.into(),
        }
    }
}

impl Default for StdinReader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Reader for StdinReader {
    type Out = String;

    async fn read(&mut self) -> Option<String> {
        if let Some(line) = self.pending.pop_front() {
            println!("\n> {line}");
            return Some(line);
        }
        print!("\n> ");
        let _ = std::io::stdout().flush();
        self.input.next_line().await.ok().flatten()
    }
}

/// 把 [`Output`] 打到 stdout 的 [`Writer`]：消息直接打印，步骤事件变成人读的行。
pub struct StdoutWriter;

impl Writer<Output> for StdoutWriter {
    fn write(&mut self, output: Output) {
        match output {
            Output::Message(line) => println!("{line}"),
            Output::Step(step) => match step {
                Step::Thought { turn, content } => println!("[{turn}] 思考：{content}"),
                Step::Answer { turn, content } => println!("\n[{turn}] 答案：{content}"),
                Step::Action {
                    turn,
                    name,
                    arguments,
                } => println!("[{turn}] 调用：{name} 参数 {arguments}"),
                Step::Observation { turn, name, output } => {
                    println!("[{turn}] {name} 返回：{output}");
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_prompts_are_drained_in_order_before_stdin() {
        let mut reader = StdinReader::with_pending(vec!["一".to_owned(), "二".to_owned()]);

        assert_eq!(reader.read().await.as_deref(), Some("一"));
        assert_eq!(reader.read().await.as_deref(), Some("二"));
    }
}
