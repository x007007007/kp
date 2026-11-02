use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

pub fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "环境变量 HOME 未设置"))
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
