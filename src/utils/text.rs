//! 文本归一化工具。

/// 归一化一行：去掉所有 `\r`，再去掉行尾空白。
///
/// 读侧与写侧必须使用同一个 `canon`，行指纹才具可比性。
pub(crate) fn canon(line: &str) -> String {
    line.replace('\r', "").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_drops_carriage_return_and_trailing_space() {
        assert_eq!(canon("let x = 1;  \r"), "let x = 1;");
        assert_eq!(canon("a\rb"), "ab");
        assert_eq!(canon("中文  "), "中文");
    }

    #[test]
    fn canon_keeps_leading_whitespace() {
        assert_eq!(canon("    indented"), "    indented");
    }
}
