//! native 引擎适配层：holder 进程的 daemon 侧视图。
//! 状态的权威在 holder 写的状态文件（runtime/sessions/<name>.json），
//! daemon 通过读文件 + 探活 holder 实现 info/list/reconcile。

use super::{Engine, EngineKind, EngineSessionInfo};
use crate::protocol::SessionSpec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// holder 维护的会话实况（状态文件 schema）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolderStatus {
    pub name: String,
    /// 真实命令 pid
    pub pid: i32,
    /// 子进程组（= 子进程 pid，daemon 据此整组杀灭）
    pub pgid: i32,
    pub holder_pid: i32,
    pub started_at_ms: u64,
    /// running | exited
    pub status: String,
    pub clients: u32,
    pub exit_code: Option<i32>,
}

pub fn sessions_runtime_dir() -> Result<PathBuf, String> {
    crate::paths::runtime_dir()
        .map(|d| d.join("sessions"))
        .map_err(|e| e.to_string())
}

pub fn session_status_path(name: &str) -> Result<PathBuf, String> {
    Ok(sessions_runtime_dir()?.join(format!("{name}.json")))
}

pub fn session_sock_path(name: &str) -> Result<PathBuf, String> {
    Ok(sessions_runtime_dir()?.join(format!("{name}.sock")))
}

pub struct PtyEngine;

impl Engine for PtyEngine {
    fn create(&self, spec: &SessionSpec, _exe: &Path) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let daemon_log = crate::paths::daemon_log().map_err(|e| e.to_string())?;
        if let Some(p) = daemon_log.parent() {
            let _ = crate::paths::ensure_dir(p);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&daemon_log)
            .map_err(|e| e.to_string())?;
        let log2 = log.try_clone().map_err(|e| e.to_string())?;
        // holder 自己 setsid；不用 process_group（否则 setsid 失败）
        Command::new(exe)
            .args(["_holder", "--spec"])
            .arg(&spec.spec_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log2))
            .stderr(std::process::Stdio::from(log))
            .spawn()
            .map_err(|e| format!("孵化 holder 失败: {e}"))?;
        // 等 holder 就绪（状态文件出现）；旧残留文件已由 kill_session 清掉，再兜底一次
        let status_path = session_status_path(&spec.name)?;
        let _ = std::fs::remove_file(&status_path);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if status_path.is_file() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.kill_session(&spec.name).ok();
        Err("holder 启动超时（详见 daemon.log）".into())
    }

    fn start_obs(&self, _spec: &SessionSpec, _exe: &Path) -> Result<(), String> {
        Ok(()) // holder 全自带：日志、退出码、元数据都在状态文件里
    }

    fn info(&self, name: &str) -> Result<Option<EngineSessionInfo>, String> {
        let Ok(p) = session_status_path(name) else {
            return Ok(None);
        };
        let Ok(text) = std::fs::read_to_string(p) else {
            return Ok(None);
        };
        let Ok(st) = serde_json::from_str::<HolderStatus>(&text) else {
            return Ok(None);
        };
        let alive = holder_alive(&st);
        if st.status == "running" && !alive {
            return Ok(None); // holder 崩溃 → 会话已死（janitor 记 session-gone）
        }
        Ok(Some(EngineSessionInfo {
            name: st.name,
            attached: st.clients as usize,
            pane_pid: st.pid,
            pane_dead: st.status == "exited",
            pane_dead_status: st.exit_code,
        }))
    }

    fn list(&self) -> Result<Vec<EngineSessionInfo>, String> {
        let dir = sessions_runtime_dir()?;
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let Some(name) = e
                    .path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                else {
                    continue;
                };
                if let Some(Some(info)) = self
                    .info(&name)
                    .ok()
                    .filter(|i| i.as_ref().map(|x| !x.pane_dead).unwrap_or(false))
                {
                    out.push(info);
                }
            }
        }
        Ok(out)
    }

    fn kill_session(&self, name: &str) -> Result<(), String> {
        let status = session_status_path(name).map_err(|e| e.to_string())?;
        let sock = session_sock_path(name).map_err(|e| e.to_string())?;
        let text = std::fs::read_to_string(&status).ok();
        let st: Option<HolderStatus> = text.and_then(|t| serde_json::from_str(&t).ok());
        if let Some(st) = &st {
            if st.status == "running" && holder_alive(st) {
                // 授权路径：Kill 帧 → holder 杀子进程组并走正常收尸
                let sent = std::os::unix::net::UnixStream::connect(&sock)
                    .and_then(|mut s| {
                        use std::io::Write;
                        let f = super::super::holder::frame(super::super::holder::F_KILL, &[]);
                        s.write_all(&f)
                    })
                    .is_ok();
                if sent {
                    // 同步等待 holder 退场（状态文件由 holder 保留、socket 先删），
                    // 否则 create 的"状态文件出现"等待会被旧文件立即满足 → 新旧 holder 混跑
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    while std::time::Instant::now() < deadline {
                        if !sock.exists() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                if sock.exists() {
                    let _ = unsafe { libc::kill(st.holder_pid, libc::SIGKILL) };
                    let _ = unsafe { libc::kill(-st.pgid, libc::SIGKILL) };
                }
            }
        }
        // 已退出 / holder 已死 / 超时兜底：清残留文件
        let _ = std::fs::remove_file(&status);
        let _ = std::fs::remove_file(&sock);
        Ok(())
    }

    fn kill_group(&self, pgid: i32, sig: i32) -> Result<(), String> {
        let r = unsafe { libc::kill(-pgid, sig) };
        if r == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(format!("kill 进程组 -{pgid}（信号 {sig}）: {err}"))
        }
    }

    fn set_status(&self, _name: &str, _status: &str) -> Result<(), String> {
        Ok(()) // 状态文件由 holder 维护
    }

    fn meta(&self, _name: &str, _key: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn pipe(&self, _name: &str, _log: &Path, _on: bool) -> Result<(), String> {
        Ok(()) // holder 自管日志
    }

    fn attach_args(&self, name: &str, readonly: bool) -> Vec<String> {
        let mut v = vec![std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "kp".into())];
        v.push("_attach".into());
        if let Ok(sock) = session_sock_path(name) {
            v.push("--socket".into());
            v.push(sock.to_string_lossy().to_string());
        }
        if readonly {
            v.push("--readonly".into());
        }
        v
    }
}

impl EngineKind {
    pub fn parse(s: &str) -> Option<EngineKind> {
        match s {
            "native" => Some(EngineKind::Native),
            "tmux" => Some(EngineKind::Tmux),
            _ => None,
        }
    }
}

fn holder_alive(st: &HolderStatus) -> bool {
    unsafe { libc::kill(st.holder_pid, 0) == 0 }
}
