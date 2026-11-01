pub mod tmux;

use crate::protocol::SessionSpec;
use std::path::Path;

/// tmux 侧一个会话的实况信息。
#[derive(Debug, Clone, Default)]
pub struct TmuxSessionInfo {
    pub name: String,
    pub attached: usize,
    pub pane_pid: i32,
    pub pane_dead: bool,
    pub pane_dead_status: Option<i32>,
}

/// 引擎抽象（决策 D7）：v0 = tmux；screen / 自研 PTY 可替换。
pub trait Engine: Send + Sync {
    /// 新建会话（detached），pane 运行 `kp _spawn --spec <path>`。
    fn create(&self, spec: &SessionSpec, exe: &Path) -> Result<(), String>;
    /// 观测项：remain-on-exit、pane-died hook、pipe-pane 日志、@kp_* 元数据。
    fn start_obs(&self, spec: &SessionSpec, exe: &Path) -> Result<(), String>;
    fn info(&self, name: &str) -> Result<Option<TmuxSessionInfo>, String>;
    /// 仅返回 kp- 前缀的会话（专用 socket 上也只有 kp 自己的）。
    fn list(&self) -> Result<Vec<TmuxSessionInfo>, String>;
    fn kill_session(&self, name: &str) -> Result<(), String>;
    /// 杀整个进程组（pgid = pane pid，FR6）。SIGTERM=15 / SIGKILL=9。
    fn kill_group(&self, pgid: i32, sig: i32) -> Result<(), String>;
    fn set_status(&self, name: &str, status: &str) -> Result<(), String>;
    fn meta(&self, name: &str, key: &str) -> Result<Option<String>, String>;
    /// pipe-pane 开关（日志限额用）。
    fn pipe(&self, name: &str, log: &Path, on: bool) -> Result<(), String>;
    /// attach 命令参数（客户端 fork+exec 执行；架构 §4.1）。
    fn attach_args(&self, name: &str, readonly: bool) -> Vec<String>;
}

pub fn engine() -> Box<dyn Engine> {
    Box::new(tmux::TmuxEngine)
}
