//! 极简 i18n（零依赖）：按运行环境选择中文/英文文案。
//!
//! 语言判定优先级：`KP_LANG` > `LC_ALL` > `LC_MESSAGES` > `LANG`，取 `_` 前的语言段
//! （`zh_CN.UTF-8` → zh）；未设置或非 zh → 英文（POSIX 惯例）。`KP_LANG` 支持显式
//! `zh` / `en`，为脚本与测试提供确定性。
//!
//! 用法：静态文案 `s("已停止", "stopped")`；带插值的在调用点直接
//! `match lang()` 或用 `f(zh, en)` 包住两个 format! 结果（本项目消息量级，
//! 双分支求值的开销可忽略，换取译文与上下文同处一地、无键名同步负担）。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

/// 显式覆盖：0 = 自动（按环境），1 = zh，2 = en（测试/嵌入式场景用）
static OVERRIDE: AtomicU8 = AtomicU8::new(0);
static AUTO: OnceLock<Lang> = OnceLock::new();

pub fn lang() -> Lang {
    match OVERRIDE.load(Ordering::Relaxed) {
        1 => return Lang::Zh,
        2 => return Lang::En,
        _ => {}
    }
    *AUTO.get_or_init(|| {
        detect_from(
            std::env::var("KP_LANG").ok(),
            std::env::var("LC_ALL").ok(),
            std::env::var("LC_MESSAGES").ok(),
            std::env::var("LANG").ok(),
        )
    })
}

/// 静态文案双语选择
pub fn s(zh: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// 已格式化的双语选择（调用点各写一个 format!）
pub fn f(zh: String, en: String) -> String {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
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

/// `zh_CN.UTF-8` → zh；`C`/`POSIX`/空 → en；未知语言段 → en
fn parse(v: &str) -> Option<Lang> {
    let code = v.split(&['.', '@', '_'][..]).next().unwrap_or("");
    match code.to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-tw" | "zh-hant" | "zh-hk" => Some(Lang::Zh),
        "c" | "posix" | "" => Some(Lang::En),
        _ => {
            // 未知语言段（fr、ja…）：暂无译文，回退英文；zh 变体已在上方覆盖
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
        // KP_LANG 最高优先
        assert_eq!(
            detect_from(
                Some("en".into()),
                Some("zh_CN".into()),
                None,
                Some("zh_CN.UTF-8".into())
            ),
            Lang::En
        );
        // LC_ALL > LC_MESSAGES > LANG
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
        // 全未设置 → POSIX 惯例英文；C/POSIX → 英文；未知语言 → 英文回退
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
        assert_eq!(
            detect_from(Some("zh_HK@latin".into()), None, None, None),
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

    #[test]
    fn selectors_pick_by_lang() {
        // 直接测 s/f 的双分支（不动全局状态）
        let saved = OVERRIDE.load(Ordering::Relaxed);
        OVERRIDE.store(1, Ordering::Relaxed);
        assert_eq!(s("是", "yes"), "是");
        OVERRIDE.store(2, Ordering::Relaxed);
        assert_eq!(s("是", "yes"), "yes");
        assert_eq!(f(format!("x{}", 1), format!("y{}", 1)), "y1");
        OVERRIDE.store(saved, Ordering::Relaxed);
    }
}
