use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::glob::glob_match;
use crate::util::{slug, to_hex};

/// 内置忽略表（FR2）：IDE/终端注入的噪音变量，不参与身份 hash。
pub const BUILTIN_IGNORED: &[&str] = &[
    // 终端通用
    "SHLVL",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "COLORTERM",
    "LANG",
    "LC_ALL",
    "LC_*",
    "COLUMNS",
    "LINES",
    "PWD",
    "OLDPWD",
    "_",
    "TMPDIR",
    "SHELL",
    // 多路复用器 / ssh
    "TMUX",
    "TMUX_PANE",
    "STY",
    "SSH_TTY",
    "SSH_AUTH_SOCK",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    // IDE 注入
    "VSCODE_*",
    "TRAE_*",
    "CURSOR_*",
    "ITERM_*",
    "WEZTERM_*",
    "KITTY_*",
    "ALACRITTY_*",
    "LaunchInstanceID",
    // macOS
    "__CF_USER_TEXT_ENCODING",
    "__CFBundleIdentifier",
    "XPC_*",
    "SECURITYSESSIONID",
    "COMMAND_MODE",
    "Apple_PubSub_Socket_Render",
];

fn is_ignored(name: &str, extra: &[String]) -> bool {
    let hit = |pat: &str| {
        if pat.contains('*') || pat.contains('?') {
            glob_match(pat, name)
        } else {
            pat == name
        }
    };
    BUILTIN_IGNORED.iter().any(|p| hit(p)) || extra.iter().any(|p| hit(p))
}

/// 身份 hash：sha256(argv⊥ + 锚定目录 + 排序后 env−忽略表) 前 8 字节十六进制（16 字符）。
pub fn hash16(
    argv: &[String],
    anchor_cwd: &Path,
    env: &BTreeMap<String, String>,
    extra_ignore: &[String],
) -> String {
    let mut h = Sha256::new();
    h.update(argv.join("\u{1f}"));
    h.update([0x1e]);
    h.update(anchor_cwd.to_string_lossy().as_bytes());
    h.update([0x1e]);
    // BTreeMap 天然按键排序
    for (k, v) in env {
        if is_ignored(k, extra_ignore) {
            continue;
        }
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(v.as_bytes());
        h.update([0x1f]);
    }
    to_hex(&h.finalize()[..8])
}

/// 生成完整会话名：kp-<项目 slug>-<hash16>。
pub fn session_name(project_dir_name: &str, hash: &str) -> String {
    format!("kp-{}-{}", slug(project_dir_name, 24), hash)
}

/// git 分支 + dirty 标记（横幅与 ls 用）。非 git 目录返回 "n/a"。
pub fn git_info(project_root: Option<&Path>) -> String {
    let Some(root) = project_root else {
        return "n/a".into();
    };
    let out = |args: &[&str]| -> Option<String> {
        let o = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        if o.status.success() {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            None
        }
    };
    let branch = out(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "n/a".into());
    let dirty = out(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if branch == "n/a" {
        branch
    } else if dirty {
        format!("{branch}*")
    } else {
        branch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn ide_noise_does_not_change_identity() {
        let argv = vec!["make".to_string(), "run".to_string()];
        let cwd = Path::new("/tmp/proj");
        let base = env_of(&[("PATH", "/usr/bin"), ("PORT", "8000")]);
        let vscode = env_of(&[
            ("PATH", "/usr/bin"),
            ("PORT", "8000"),
            ("VSCODE_CWD", "/app"),
            ("TERM_PROGRAM", "vscode"),
            ("LC_0", "x"),
        ]);
        let a = hash16(&argv, cwd, &base, &[]);
        let b = hash16(&argv, cwd, &vscode, &[]);
        assert_eq!(a, b, "IDE 噪音变量必须被忽略表吸收");
    }

    #[test]
    fn real_env_change_changes_identity() {
        let argv = vec!["make".to_string()];
        let cwd = Path::new("/tmp/proj");
        let a = hash16(&argv, cwd, &env_of(&[("PORT", "8000")]), &[]);
        let b = hash16(&argv, cwd, &env_of(&[("PORT", "8001")]), &[]);
        assert_ne!(a, b);
    }

    #[test]
    fn extra_ignore_works() {
        let argv = vec!["x".to_string()];
        let cwd = Path::new("/tmp/proj");
        let a = hash16(&argv, cwd, &env_of(&[("MY_TOKEN", "aaa")]), &[]);
        let b = hash16(&argv, cwd, &env_of(&[("MY_TOKEN", "bbb")]), &[]);
        assert_ne!(a, b);
        let c = hash16(
            &argv,
            cwd,
            &env_of(&[("MY_TOKEN", "aaa")]),
            &["MY_TOKEN".into()],
        );
        let d = hash16(
            &argv,
            cwd,
            &env_of(&[("MY_TOKEN", "bbb")]),
            &["MY_TOKEN".into()],
        );
        assert_eq!(c, d, "用户忽略表生效");
    }

    #[test]
    fn session_name_format() {
        let n = session_name("我的项目", "abcdef1234567890");
        assert!(n.starts_with("kp-"), "前缀 kp-: {n}");
        assert!(n.ends_with("-abcdef1234567890"));
        assert!(n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        let n2 = session_name("abc", "hash");
        assert_eq!(n2, "kp-abc-hash");
    }

    #[test]
    fn git_info_non_git_dir() {
        let d = std::env::temp_dir().join(format!(
            "kp-git-{}-{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(git_info(Some(&d)), "n/a");
        assert_eq!(git_info(None), "n/a");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn argv_difference_changes_identity() {
        let cwd = Path::new("/tmp/proj");
        let env = env_of(&[("PATH", "/usr/bin")]);
        let a = hash16(&["run".to_string(), "8080".to_string()], cwd, &env, &[]);
        let b = hash16(&["run".to_string(), "8081".to_string()], cwd, &env, &[]);
        assert_ne!(a, b);
    }
}
