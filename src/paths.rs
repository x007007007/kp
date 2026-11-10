use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

pub fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{}HOME",
                crate::i18n::s("环境变量未设置: ", "environment variable not set: ")
            ),
        )
    })
}

fn xdg_dir(var: &str, fallback: &str) -> io::Result<PathBuf> {
    if let Some(v) = std::env::var_os(var) {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    Ok(home()?.join(fallback))
}

/// 状态目录：$XDG_STATE_HOME/kp，缺省 ~/.local/state/kp（FR8）。
pub fn state_dir() -> io::Result<PathBuf> {
    Ok(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("kp"))
}

pub fn config_dir() -> io::Result<PathBuf> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("kp"))
}

/// 运行时目录：$XDG_RUNTIME_DIR/kp，缺省按用户隔离的临时目录（FR7）。
pub fn runtime_dir() -> io::Result<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v).join("kp"));
        }
    }
    let mut d = std::env::temp_dir();
    d.push(format!("kp-{}", unsafe { libc::getuid() }));
    Ok(d)
}

pub fn ensure_dir(p: &std::path::Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(p)
}

pub fn socket_path() -> io::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

pub fn logs_dir() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("logs"))
}

pub fn specs_dir() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("specs"))
}

pub fn sessions_file() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("sessions.json"))
}

pub fn history_file() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("history.jsonl"))
}

/// 策略粘性存储：会话名 → 生效策略（D11）。
pub fn policies_file() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("policies.json"))
}

pub fn daemon_log() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("daemon.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// env 是进程全局的：本测试独占使用后恢复，不与其他测试并发冲突
    /// （同 crate 其余单测都用显式路径参数，不读这些 env）。
    #[test]
    fn xdg_resolution_and_fallback() {
        let saved_state = std::env::var("XDG_STATE_HOME").ok();
        let saved_rt = std::env::var("XDG_RUNTIME_DIR").ok();

        std::env::set_var("XDG_STATE_HOME", "/tmp/kp-px-state");
        assert_eq!(state_dir().unwrap(), PathBuf::from("/tmp/kp-px-state/kp"));
        std::env::remove_var("XDG_STATE_HOME");
        assert!(state_dir().unwrap().ends_with(".local/state/kp"));

        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/kp-px-run");
        assert_eq!(runtime_dir().unwrap(), PathBuf::from("/tmp/kp-px-run/kp"));
        std::env::remove_var("XDG_RUNTIME_DIR");
        // 缺省：用户隔离的临时目录
        let fallback = runtime_dir().unwrap();
        assert!(fallback.starts_with(std::env::temp_dir()));
        assert!(fallback.to_string_lossy().contains("kp-"));

        assert!(socket_path().unwrap().ends_with("daemon.sock"));
        assert!(logs_dir().unwrap().ends_with("logs"));
        assert!(sessions_file().unwrap().ends_with("sessions.json"));
        assert!(history_file().unwrap().ends_with("history.jsonl"));

        match saved_state {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        match saved_rt {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn ensure_dir_is_private() {
        let d = std::env::temp_dir().join(format!(
            "kp-mkdir-{}-{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        ensure_dir(&d).unwrap();
        ensure_dir(&d).unwrap(); // 幂等
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&d).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "运行时目录应 0700");
        let _ = std::fs::remove_dir_all(d);
    }
}
