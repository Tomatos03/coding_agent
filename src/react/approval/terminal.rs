//! 终端实现：本 crate 内置的唯一询问方式，直接读写终端。

use std::io::Write as _;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use super::{ApprovalRequest, Confirmer, Decision};

/// 终端确认方：从终端（tokio stdin）读取 y / n。
///
/// 输入源固定为终端——需要其它输入方式（GUI、远程）时实现 [`Confirmer`]，
/// 或用 [`FnConfirmer`](super::FnConfirmer) 包一层。
/// 未做出选择就一直等待；EOF 或读取失败表示无人可答，返回 [`Decision::Pending`]。
#[derive(Default)]
pub struct TerminalConfirmer;

impl TerminalConfirmer {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Confirmer for TerminalConfirmer {
    async fn confirm(&self, request: &ApprovalRequest) -> Decision {
        print_request(request);
        let mut input = BufReader::new(tokio::io::stdin());
        ask_until_decided(&mut input).await
    }
}

/// 询问循环：反复读行直到拿到明确的 y / n；EOF / 读取失败 → 挂起。
async fn ask_until_decided<R>(input: &mut R) -> Decision
where
    R: AsyncBufRead + Unpin,
{
    loop {
        print!("       选择 [y] 批准 / [n] 拒绝：");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match input.read_line(&mut line).await {
            Ok(0) | Err(_) => return Decision::Pending,
            Ok(_) => match parse_yn(line.trim()) {
                Some(decision) => return decision,
                None => println!("       请输入 y 或 n"),
            },
        }
    }
}

/// 终端词汇：y = 确认，n = 拒绝；其余都是「尚未做出选择」。
fn parse_yn(line: &str) -> Option<Decision> {
    match line {
        "y" | "Y" => Some(Decision::Approve),
        "n" | "N" => Some(Decision::Deny),
        _ => None,
    }
}

fn print_request(request: &ApprovalRequest) {
    println!();
    println!("[审批] 工具 `{}` 请求执行", request.tool);
    if !request.description.is_empty() {
        println!("       说明：{}", request.description);
    }
    println!("       参数：{}", request.arguments);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_vocabulary_is_strict_y_or_n() {
        assert_eq!(parse_yn("y"), Some(Decision::Approve));
        assert_eq!(parse_yn("N"), Some(Decision::Deny));
        assert_eq!(parse_yn("yes"), None);
        assert_eq!(parse_yn(""), None);
    }

    #[tokio::test]
    async fn terminal_maps_y_and_n() {
        let mut input = BufReader::new(&b"y\n"[..]);
        assert_eq!(ask_until_decided(&mut input).await, Decision::Approve);

        let mut input = BufReader::new(&b" N \n"[..]);
        assert_eq!(ask_until_decided(&mut input).await, Decision::Deny);
    }

    #[tokio::test]
    async fn terminal_keeps_waiting_on_invalid_input() {
        let mut input = BufReader::new(&b"\nx\nY\n"[..]);
        assert_eq!(ask_until_decided(&mut input).await, Decision::Approve);
    }

    #[tokio::test]
    async fn terminal_eof_suspends() {
        let mut input = BufReader::new(&b""[..]);
        assert_eq!(ask_until_decided(&mut input).await, Decision::Pending);
    }
}
