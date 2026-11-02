use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

pub const PROTO_VERSION: u32 = 2;
const MAX_FRAME: usize = 32 * 1024 * 1024;

/// 重复策略（FR3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Attach,
    Wait,
    Restart,
    New,
    Reject,
}

/// 客户端解析配置后生成的会话规格；daemon 不读配置文件（04-config §2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub name: String,
    pub argv: Vec<String>,
    /// 进程工作目录 = 调用目录（身份锚定的项目根另存于 project_root）。
    pub cwd: PathBuf,
    /// 启动时全量环境快照，restart 默认复用（FR2）。
    pub env: BTreeMap<String, String>,
    /// 配置解析出的策略（service > run.default_policy），粘性判定的兜底
    pub policy: Policy,
    /// CLI -p 显式指定的策略：粘性判定最高优先级；daemon 消费后清空
    #[serde(default)]
    pub policy_cli: Option<Policy>,
    pub log_path: PathBuf,
    pub spec_path: PathBuf,
    pub branch: String,
    pub project_root: Option<PathBuf>,
    pub identity_hash: String,
    pub term_timeout: u64,
    pub log_max_mb: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub branch: String,
    pub policy: Option<Policy>,
    /// running | exited
    pub status: String,
    pub started_at_ms: Option<u64>,
    pub ended_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub reason: String,
    pub pane_pid: Option<i32>,
    pub clients: usize,
    pub log_path: String,
    pub identity_hash: String,
    pub project_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitInfo {
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// exited | stopped | restart | session-gone（restart 时客户端自动跟随新实例）
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpawnOutcome {
    Started {
        info: SessionInfo,
        attach_args: Vec<String>,
        writable: bool,
    },
    Attached {
        info: SessionInfo,
        attach_args: Vec<String>,
        writable: bool,
    },
    Restarted {
        info: SessionInfo,
        attach_args: Vec<String>,
        writable: bool,
        old_exit: Option<i32>,
    },
    Waiting {
        info: SessionInfo,
    },
    Rejected {
        info: SessionInfo,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
    },
    Ping,
    Spawn {
        spec: SessionSpec,
    },
    AttachExisting {
        name: String,
    },
    RestartByName {
        name: String,
        fresh_env: Option<BTreeMap<String, String>>,
    },
    Stop {
        name: String,
    },
    StopAll,
    List,
    Status {
        name: String,
    },
    WaitExit {
        name: String,
        timeout_ms: u64,
    },
    ReportDone {
        name: String,
        exit_code: Option<i32>,
    },
    Clean {
        history_keep: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    Pong,
    Spawn(SpawnOutcome),
    Attach(SpawnOutcome),
    Exit(Option<ExitInfo>),
    Sessions(Vec<SessionInfo>),
    Detail(Option<SessionInfo>),
    Stopped(Vec<(String, Option<i32>)>),
    Cleaned,
    Ok,
    Err { code: i32, message: String },
}

pub fn send_frame<T: Serialize>(stream: &mut UnixStream, msg: &T) -> io::Result<()> {
    use std::io::Write;
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "帧过大"));
    }
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

pub fn recv_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
    use std::io::Read;
    let mut lenb = [0u8; 4];
    stream.read_exact(&mut lenb)?;
    let len = u32::from_be_bytes(lenb) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "帧过大"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}
