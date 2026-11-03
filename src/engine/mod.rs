pub mod native;
pub mod tmux;

use crate::protocol::SessionSpec;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 引擎侧一个会话的实况信息。
#[derive(Debug, Clone, Default)]
pub struct EngineSessionInfo {
    pub name: String,
    pub attached: usize,
    pub pane_pid: i32,
    pub pane_dead: bool,
    pub pane_dead_status: Option<i32>,
}

/// 引擎种类（D12）：native = 自研 PTY holder；tmux = tmux 适配层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Native,
    Tmux,
}

/// 旧 spec/status 文件缺 engine 字段时的缺省（历史会话都是 tmux 引擎产物）。
pub fn default_engine_tmux() -> EngineKind {
    EngineKind::Tmux
}

/// 引擎抽象（D7/D12）：native 与 tmux 双实现并存，按会话分派；screen 经评估否决（无退出码/无只读 attach），trait 保留未来扩展。
pub trait Engine: Send + Sync {
    /// 新建会话（detached）。tmux：new-session 跑 `kp _spawn`；native：孵化 holder 进程。
    fn create(&self, spec: &SessionSpec, exe: &Path) -> Result<(), String>;
    /// 观测项设置。tmux：remain-on-exit/hook/pipe-pane/@kp_*；native：holder 自带，no-op。
    fn start_obs(&self, spec: &SessionSpec, exe: &Path) -> Result<(), String>;
    fn info(&self, name: &str) -> Result<Option<EngineSessionInfo>, String>;
    /// 仅返回本引擎在管的 live 会话。
    fn list(&self) -> Result<Vec<EngineSessionInfo>, String>;
    fn kill_session(&self, name: &str) -> Result<(), String>;
    /// 杀整个进程组（SIGTERM=15 / SIGKILL=9），两引擎同为实现。
    fn kill_group(&self, pgid: i32, sig: i32) -> Result<(), String>;
    fn set_status(&self, name: &str, status: &str) -> Result<(), String>;
    fn meta(&self, name: &str, key: &str) -> Result<Option<String>, String>;
    /// pipe-pane 开关（仅 tmux；native 的日志由 holder 自管并自带限额）。
    fn pipe(&self, name: &str, log: &Path, on: bool) -> Result<(), String>;
    /// attach 命令参数（客户端 fork+exec 执行；架构 §4.1）。
    fn attach_args(&self, name: &str, readonly: bool) -> Vec<String>;
}

pub fn engine(kind: EngineKind) -> Box<dyn Engine> {
    match kind {
        EngineKind::Native => Box::new(native::PtyEngine),
        EngineKind::Tmux => Box::new(tmux::TmuxEngine),
    }
}
