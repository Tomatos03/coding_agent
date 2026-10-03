//! 内容哈希工具。
//!
//! 用 FNV-1a 64 位而不是密码学哈希：这里只需要「内容有没有变」的快速指纹，
//! 且不引入额外依赖；算法在代码里固定，同一内容永远得到同一个值。
//! 它不是安全哈希，不应用于防篡改场景。

use crate::utils::text::canon;

/// 计算一段文本内容的 FNV-1a 64 位哈希。
///
/// 步骤流程：
/// 1. 状态初始化为 offset basis（`OFFSET_BASIS`，`0xcbf2_9ce4_8422_2325`）；
/// 2. 逐字节处理：先异或进状态，再乘以质数 `PRIME`（`0x0000_0100_0000_01b3`），
///    乘法按 2^64 取模回绕——「先异或、后乘」正是 1a 与 FNV-1 的区别；
/// 3. 所有字节处理完，状态即最终哈希值。
pub(crate) fn hash64(content: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    hash
}

/// 行指纹：先归一化再哈希，是判断「行内容是否变化」的唯一依据。
///
/// 与锚点不同：它不取模、不做冲突消解，因此只依赖这一行自身的内容。
pub(crate) fn line_fingerprint(line: &str) -> u64 {
    hash64(&canon(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_fingerprint_only_depends_on_own_content() {
        assert_eq!(line_fingerprint("aa"), line_fingerprint("aa"));
        assert_ne!(line_fingerprint("aa"), line_fingerprint("bb"));
        // canon 吸收 `\r` 与行尾空白。
        assert_eq!(line_fingerprint("aa"), line_fingerprint("aa\r"));
        assert_eq!(line_fingerprint("aa"), line_fingerprint("aa   "));
    }
}
