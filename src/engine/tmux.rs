use super::{Engine, EngineSessionInfo};
use crate::i18n;
use crate::protocol::SessionSpec;
use crate::util::{now_ms, sh_quote};
use std::path::Path;
use std::process::Command;

/// v0 引擎：tmux，一律专用 socket `-L kp` 并以 `-f /dev/null` 屏蔽用户配置，
/// 保证会话名空间、server 生命周期与用户自己的 tmux 完全隔离（架构 A1）。
pub struct TmuxEngine;

const SOCKET: &str = "kp";
const FMT: &str =
    "#{pane_pid}|#{pane_dead}|#{pane_dead_status}|#{session_created}|#{session_attached}";
const LIST_FMT: &str = "#{session_name}|#{session_created}|#{session_attached}";

fn call(args: &[&str]) -> Result<String, String> {
    let mut full: Vec<&str> = vec!["-L", SOCKET, "-f", "/dev/null"];
    full.extend(args);
    let out = Command::new("tmux").args(&full).output().map_err(|e| {
        i18n::f(
            format!("执行 tmux 失败（是否已安装？macOS: brew install tmux）: {e}"),
            format!("failed to run tmux (is it installed? macOS: brew install tmux): {e}"),
        )
    })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn session_missing(err: &str) -> bool {
    err.contains("can't find session") || err.contains("Can't find session")
}

fn no_server(err: &str) -> bool {
    err.contains("no server running") || err.contains("error connecting")
}

fn parse_info(name: &str, line: &str) -> Option<EngineSessionInfo> {
    let parts: Vec<&str> = line.splitn(5, '|').collect();
    if parts.len() < 5 {
        return None;
    }
    Some(EngineSessionInfo {
        name: name.to_string(),
        pane_pid: parts[0].trim().parse().unwrap_or(0),
        pane_dead: parts[1].trim() == "1",
        pane_dead_status: parts[2].trim().parse().ok(),
        attached: parts[4].trim().parse().unwrap_or(0),
    })
}

impl Engine for TmuxEngine {
    fn create(&self, spec: &SessionSpec, exe: &Path) -> Result<(), String> {
        let cmd = format!(
            "{} _spawn --spec {}",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&spec.spec_path.to_string_lossy())
        );
        call(&[
            "new-session",
            "-d",
            "-s",
            &spec.name,
            "-c",
            &spec.cwd.to_string_lossy(),
            &cmd,
        ])
        .map(|_| ())
    }

    fn start_obs(&self, spec: &SessionSpec, exe: &Path) -> Result<(), String> {
        // 进程退出后保留死 pane（可读退出码、可观测），随后由 daemon 清理会话。
        call(&[
            "set-option",
            "-w",
            "-t",
            &format!("{}:0", spec.name),
            "remain-on-exit",
            "on",
        ])?;
        // pane-died hook → 事件驱动上报退出码（架构 A5）。
        let report = format!(
            "{} _report {} #{{pane_dead_status}}",
            sh_quote(&exe.to_string_lossy()),
            spec.name
        );
        let hook = format!("run-shell -b {}", sh_quote(&report));
        call(&[
            "set-hook",
            "-p",
            "-t",
            &format!("{}:0.0", spec.name),
            "pane-died",
            &hook,
        ])?;
        self.pipe(&spec.name, &spec.log_path, true)?;
        self.set_status(&spec.name, "running")?;
        for (k, v) in [
            ("@kp_branch", spec.branch.clone()),
            ("@kp_started", now_ms().to_string()),
            ("@kp_cwd", spec.cwd.to_string_lossy().to_string()),
            ("@kp_argv", spec.argv.join("\u{1f}")),
            ("@kp_log", spec.log_path.to_string_lossy().to_string()),
        ] {
            call(&["set-option", "-t", &spec.name, k, &v])?;
        }
        Ok(())
    }

    fn info(&self, name: &str) -> Result<Option<EngineSessionInfo>, String> {
        match call(&["display-message", "-p", "-t", &format!("{name}:0"), FMT]) {
            Ok(s) => Ok(parse_info(name, s.trim())),
            Err(e) if session_missing(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn list(&self) -> Result<Vec<EngineSessionInfo>, String> {
        match call(&["list-sessions", "-F", LIST_FMT]) {
            Ok(s) => Ok(s
                .lines()
                .filter_map(|l| {
                    let mut it = l.splitn(3, '|');
                    let name = it.next()?.to_string();
                    if !name.starts_with("kp-") {
                        return None;
                    }
                    parse_info(
                        &name,
                        &format!(
                            "0|0||{}|{}",
                            it.next().unwrap_or("0"),
                            it.next().unwrap_or("0")
                        ),
                    )
                })
                .collect()),
            Err(e) if no_server(&e) || session_missing(&e) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn kill_session(&self, name: &str) -> Result<(), String> {
        match call(&["kill-session", "-t", name]) {
            Ok(_) => Ok(()),
            Err(e) if session_missing(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn kill_group(&self, pgid: i32, sig: i32) -> Result<(), String> {
        let r = unsafe { libc::kill(-pgid, sig) };
        if r == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(()) // 组已不存在
        } else {
            Err(i18n::f(
                format!("kill 进程组 -{pgid}（信号 {sig}）: {err}"),
                format!("failed to kill process group -{pgid} (signal {sig}): {err}"),
            ))
        }
    }

    fn set_status(&self, name: &str, status: &str) -> Result<(), String> {
        call(&["set-option", "-t", name, "@kp_status", status]).map(|_| ())
    }

    fn meta(&self, name: &str, key: &str) -> Result<Option<String>, String> {
        match call(&["show-options", "-v", "-t", name, key]) {
            Ok(s) => {
                let v = s.trim().to_string();
                Ok(if v.is_empty() { None } else { Some(v) })
            }
            Err(e) if session_missing(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn pipe(&self, name: &str, log: &Path, on: bool) -> Result<(), String> {
        let tgt = format!("{name}:0.0");
        if on {
            let cmd = format!("cat >> {}", sh_quote(&log.to_string_lossy()));
            call(&["pipe-pane", "-o", "-t", &tgt, &cmd]).map(|_| ())
        } else {
            call(&["pipe-pane", "-t", &tgt]).map(|_| ())
        }
    }

    fn attach_args(&self, name: &str, readonly: bool) -> Vec<String> {
        let mut v = vec![
            "tmux".to_string(),
            "-L".into(),
            SOCKET.into(),
            "attach".into(),
        ];
        if readonly {
            v.push("-r".into());
        }
        v.push("-t".into());
        v.push(name.to_string());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_info_full() {
        let i = parse_info("kp-x-1", "123|0||1700000000|2").unwrap();
        assert_eq!(i.name, "kp-x-1");
        assert_eq!(i.pane_pid, 123);
        assert!(!i.pane_dead);
        assert_eq!(i.pane_dead_status, None);
        assert_eq!(i.attached, 2);
    }

    #[test]
    fn parse_info_dead_with_status() {
        let i = parse_info("kp-x", "99|1|130|0|0").unwrap();
        assert!(i.pane_dead);
        assert_eq!(i.pane_dead_status, Some(130));
        assert_eq!(i.attached, 0);
    }

    #[test]
    fn parse_info_garbage() {
        assert!(parse_info("kp-x", "garbage").is_none());
        assert!(parse_info("kp-x", "").is_none());
    }

    #[test]
    fn error_matchers() {
        assert!(session_missing("Can't find session kp-x"));
        assert!(session_missing("can't find session kp-x"));
        assert!(!session_missing("no server running on /tmp/x"));
        assert!(no_server("no server running on /tmp/x"));
        assert!(no_server("error connecting to /tmp/x: No such file"));
        assert!(!no_server("Can't find session kp-x"));
    }
}
