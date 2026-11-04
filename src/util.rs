use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 单引号 shell 转义（tmux 命令串/run-shell 同样适用）。
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 只保留 [A-Za-z0-9_-]，其余替换为 '-'；用于会话名片段。
pub fn slug(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(max); // 经上一步映射后必为 ASCII，按字节截断安全
    if out.is_empty() {
        out.push('x');
    }
    out
}

pub fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_units() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(59_000), "59s");
        assert_eq!(fmt_duration(61_000), "1m1s");
        assert_eq!(fmt_duration(3_600_000), "1h0m");
        assert_eq!(fmt_duration(3_720_000), "1h2m");
    }

    #[test]
    fn slug_maps_non_ascii_and_caps_length() {
        assert_eq!(slug("abc-DEF_1", 10), "abc-DEF_1");
        assert_eq!(slug("我的项目", 10), "----");
        assert_eq!(slug("abcdefghijklmn", 4), "abcd");
        assert_eq!(slug("", 5), "x");
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("a b"), "'a b'");
        // it's → 'it'\''s'（POSIX 单引号转义标准写法）
        let q = sh_quote("it's");
        assert!(q.starts_with('\'') && q.ends_with('\''));
        assert!(q.contains("'\\''"), "actual: {q}");
    }

    #[test]
    fn to_hex_lowercase() {
        assert_eq!(to_hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn now_ms_monotonic() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a);
        assert!(a > 1_700_000_000_000); // 2023 之后
    }
}
