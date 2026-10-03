//! 锚点编解码工具。
//!
//! 锚点是 4 个字母，编码 `52^4` 空间里的一个槽位。这里只做纯编解码；
//! 「槽位冲突消解（开放寻址 + 线性探测）」属于分配逻辑，放在
//! [`crate::tools::local::anchor_registry`]。

/// 锚点长度（字符数）。
pub const ANCHOR_LEN: usize = 4;
/// 锚点与正文之间的分隔符。
pub const ANCHOR_SEP: char = '│';

const ALPHABET: &[u8; 52] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// 字母表基数。
pub const RADIX: usize = 52;
/// 锚点池大小：`52^4`。
pub const ANCHOR_SPACE: usize = RADIX * RADIX * RADIX * RADIX;
/// 线性探测步长；取素数且与 `ANCHOR_SPACE`（`2^8 * 13^4`）互质，探测可走遍全表。
pub const PROBE_STRIDE: usize = 836_291;

/// 把锚点池下标编码成固定长度的字母串。
pub(crate) fn encode_anchor(mut index: usize) -> String {
    let mut chars = [b'A'; ANCHOR_LEN];
    for slot in chars.iter_mut().rev() {
        *slot = ALPHABET[index % RADIX];
        index /= RADIX;
    }
    String::from_utf8(chars.to_vec()).expect("锚点字母表是 ASCII")
}

/// 把 4 字母锚点解码回池下标；长度或字符不合法时返回 `None`。
pub(crate) fn decode_anchor(anchor: &str) -> Option<usize> {
    if anchor.len() != ANCHOR_LEN {
        return None;
    }

    let mut index = 0usize;
    for byte in anchor.bytes() {
        let digit = ALPHABET.iter().position(|candidate| *candidate == byte)?;
        index = index * RADIX + digit;
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        for index in [0usize, 1, 51, 52, 1234, ANCHOR_SPACE - 1] {
            let anchor = encode_anchor(index);
            assert_eq!(anchor.len(), ANCHOR_LEN);
            assert_eq!(decode_anchor(&anchor), Some(index));
        }
    }

    #[test]
    fn anchors_are_four_ascii_letters() {
        for index in [0usize, 7, 100, 9999, ANCHOR_SPACE - 1] {
            let anchor = encode_anchor(index);
            assert_eq!(anchor.len(), ANCHOR_LEN);
            assert!(anchor.chars().all(|c| c.is_ascii_alphabetic()));
        }
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert_eq!(decode_anchor("abc"), None);
        assert_eq!(decode_anchor("abcde"), None);
        assert_eq!(decode_anchor("ab1d"), None);
        assert_eq!(decode_anchor("ab_d"), None);
    }

    #[test]
    fn probe_stride_is_coprime_with_anchor_space() {
        fn gcd(mut a: usize, mut b: usize) -> usize {
            while b != 0 {
                let t = a % b;
                a = b;
                b = t;
            }
            a
        }
        assert_eq!(gcd(PROBE_STRIDE, ANCHOR_SPACE), 1);
    }
}
