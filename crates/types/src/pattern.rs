//! Key 的共享字节通配规则；Shared byte-glob rules for keys.

/// 按 UTF-8 原始字节匹配完整 Key，使用常数额外空间。
/// Matches a complete key's raw UTF-8 bytes with constant extra space.
///
/// `*` 匹配任意字节序列，`?` 匹配单字节，反斜杠转义下一字节。
/// `*` matches any byte sequence, `?` one byte, and backslash escapes the next byte.
/// 调用方须在接受 pattern 时拒绝末尾单独反斜杠。
/// Callers must reject a dangling backslash when accepting a pattern.
pub fn key_pattern_matches(pattern: &str, key: &str) -> bool {
    let (p, k) = (pattern.as_bytes(), key.as_bytes());
    let (mut pi, mut ki, mut star, mut retry) = (0, 0, None, 0);
    while ki < k.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi + 1);
            pi += 1;
            // 每个星号只能从其所在位置向前重试，不能重用前一星号的匹配片段。
            // Each star retries from its own position, never reusing an earlier star's matched fragment.
            retry = ki;
        } else {
            let (literal, advance) = if pi < p.len() && p[pi] == b'\\' {
                (p.get(pi + 1).copied(), 2)
            } else {
                (p.get(pi).copied(), 1)
            };
            if literal == Some(k[ki]) || (pi < p.len() && p[pi] == b'?' && advance == 1) {
                pi += advance;
                ki += 1;
            } else if let Some(after_star) = star {
                retry += 1;
                ki = retry;
                pi = after_star;
            } else {
                return false;
            }
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::key_pattern_matches;

    /// 空串和星号无需非空 Key 才能定义；Empty strings and stars have defined matching semantics.
    #[test]
    fn empty_and_literal_byte_boundaries() {
        for (pattern, key, expected) in [
            ("", "", true),
            ("*", "", true),
            ("**", "", true),
            ("?", "", false),
            ("", "a", false),
            ("*xy*yz", "xyz", false),
            ("*xy*yz", "xyyz", true),
            ("a**b", "ab", true),
            ("?", "é", false),
            ("??", "é", true),
            (r"\*", "*", true),
            (r"\?", "?", true),
            (r"\?", "a", false),
        ] {
            assert_eq!(
                key_pattern_matches(pattern, key),
                expected,
                "{pattern:?}, {key:?}"
            );
        }
    }
}
