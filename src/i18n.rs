//! i18n 接线层：环境检测（KP_LANG > LC_ALL > LC_MESSAGES > LANG）→ rust-i18n set_locale。
//! 译文资源在 locales/app.yml（_version: 2，zh/en 同文件并排，便于对照补翻），
//! 编译期内嵌进二进制；缺键回退 en（见 lib.rs 的 i18n! 配置）。

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

static INIT: OnceLock<()> = OnceLock::new();

/// 进程入口调用一次（kp/kpm main）。
pub fn init() {
    INIT.get_or_init(|| {
        let lang = detect_from(
            std::env::var("KP_LANG").ok(),
            std::env::var("LC_ALL").ok(),
            std::env::var("LC_MESSAGES").ok(),
            std::env::var("LANG").ok(),
        );
        rust_i18n::set_locale(match lang {
            Lang::Zh => "zh",
            Lang::En => "en",
        });
    });
}

/// 纯函数便于测试：按 POSIX 语义解析 locale 链
pub fn detect_from(
    kp_lang: Option<String>,
    lc_all: Option<String>,
    lc_messages: Option<String>,
    lang: Option<String>,
) -> Lang {
    if let Some(v) = kp_lang {
        return parse(&v).unwrap_or(Lang::En);
    }
    for v in [lc_all, lc_messages, lang].into_iter().flatten() {
        if !v.is_empty() {
            return parse(&v).unwrap_or(Lang::En);
        }
    }
    Lang::En
}

/// `zh_CN.UTF-8` → zh；`C`/`POSIX`/空 → en；未知语言段 → en 回退
fn parse(v: &str) -> Option<Lang> {
    let code = v.split(&['.', '@', '_'][..]).next().unwrap_or("");
    match code.to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-tw" | "zh-hant" | "zh-hk" => Some(Lang::Zh),
        "c" | "posix" | "" => Some(Lang::En),
        _ => {
            if code.to_ascii_lowercase().starts_with("zh") {
                Some(Lang::Zh)
            } else {
                Some(Lang::En)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_chain_priority() {
        assert_eq!(
            detect_from(
                Some("en".into()),
                Some("zh_CN".into()),
                None,
                Some("zh_CN.UTF-8".into())
            ),
            Lang::En
        );
        assert_eq!(
            detect_from(
                None,
                Some("en_US".into()),
                Some("zh_CN".into()),
                Some("zh_CN".into())
            ),
            Lang::En
        );
        assert_eq!(
            detect_from(None, None, Some("zh_CN.UTF-8".into()), Some("en_US".into())),
            Lang::Zh
        );
    }

    #[test]
    fn locale_defaults_to_english() {
        assert_eq!(detect_from(None, None, None, None), Lang::En);
        assert_eq!(detect_from(None, Some("C".into()), None, None), Lang::En);
        assert_eq!(
            detect_from(None, Some("POSIX".into()), None, None),
            Lang::En
        );
        assert_eq!(
            detect_from(None, Some("fr_FR.UTF-8".into()), None, None),
            Lang::En
        );
    }

    #[test]
    fn zh_variants_recognized() {
        assert_eq!(detect_from(Some("zh".into()), None, None, None), Lang::Zh);
        assert_eq!(
            detect_from(Some("zh_CN".into()), None, None, None),
            Lang::Zh
        );
        assert_eq!(
            detect_from(Some("zh_TW.UTF-8".into()), None, None, None),
            Lang::Zh
        );
        assert_eq!(detect_from(Some("ZH".into()), None, None, None), Lang::Zh);
    }

    #[test]
    fn invalid_kp_lang_falls_back_to_english() {
        assert_eq!(
            detect_from(Some("klingon".into()), Some("zh_CN".into()), None, None),
            Lang::En
        );
        assert_eq!(detect_from(Some("".into()), None, None, None), Lang::En);
    }

    /// 全量覆盖：扫描源码里所有 t!("key") 引用，逐键验证 zh/en 都有译文。
    /// 防"运行时打印出键名"——译文缺失在编译期测试就拦下，而非用户终端。
    #[test]
    fn translations_cover_every_referenced_key() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let mut keys = std::collections::BTreeSet::new();
        let mut stack = vec![std::path::PathBuf::from(manifest).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|e| e == "rs").unwrap_or(false) {
                    let text = std::fs::read_to_string(&p).unwrap();
                    let mut i = 0;
                    while let Some(pos) = text[i..].find("t!(\"") {
                        let at = i + pos;
                        // 跳过 format!( 等尾部恰好含 t!( 字样的非调用（词边界检查）
                        if at > 0 {
                            let prev = text.as_bytes()[at - 1];
                            if prev.is_ascii_alphanumeric() || prev == b'_' {
                                i = at + 4;
                                continue;
                            }
                        }
                        // 行内注释里的 t!( 字样不算引用（含本测试自己的注释）
                        let line_start = text[..at].rfind('\n').map(|x| x + 1).unwrap_or(0);
                        let before = &text[line_start..at];
                        if let Some(c) = before.find("//") {
                            if before[..c].matches('"').count() % 2 == 0 {
                                i = at + 4;
                                continue;
                            }
                        }
                        let start = at + 4;
                        let end = text[start..].find('"').unwrap() + start;
                        keys.insert(text[start..end].to_string());
                        i = end;
                    }
                }
            }
        }
        assert!(keys.len() > 100, "t! 键扫描异常（仅 {} 个）", keys.len());
        for key in keys {
            for loc in ["zh", "en"] {
                rust_i18n::set_locale(loc);
                let s = rust_i18n::t!(&key);
                assert_ne!(&*s, &key, "locale {loc} 缺键 {key}");
            }
        }
    }
}
