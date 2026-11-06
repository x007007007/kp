use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::engine::{default_engine_tmux, EngineKind};

pub const PROTO_VERSION: u32 = 3;
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
    /// 会话引擎（D12）；旧文件缺省为 tmux
    #[serde(default = "default_engine_tmux")]
    pub engine: EngineKind,
    pub log_path: PathBuf,
    pub spec_path: PathBuf,
    pub branch: String,
    pub project_root: Option<PathBuf>,
    pub identity_hash: String,
    pub term_timeout: u64,
    pub log_max_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 引擎种类（native | tmux）
    #[serde(default = "crate::engine::default_engine_tmux")]
    pub engine: crate::engine::EngineKind,
}

impl Default for SessionInfo {
    fn default() -> Self {
        SessionInfo {
            name: String::new(),
            argv: Vec::new(),
            cwd: String::new(),
            branch: String::new(),
            policy: None,
            status: String::new(),
            started_at_ms: None,
            ended_at_ms: None,
            exit_code: None,
            reason: String::new(),
            pane_pid: None,
            clients: 0,
            log_path: String::new(),
            identity_hash: String::new(),
            project_root: String::new(),
            engine: crate::engine::default_engine_tmux(),
        }
    }
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
    /// 排障信息（kpm doctor）：daemon 自述身份
    DaemonInfo,
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
    /// daemon 自述：pid / 可执行文件 / 版本 / 启动时间（kpm doctor）
    DaemonInfo {
        pid: i32,
        exe: String,
        version: String,
        started_at_ms: u64,
    },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_response_frame_roundtrip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        send_frame(&mut a, &Request::Ping).unwrap();
        let req: Request = recv_frame(&mut b).unwrap();
        assert!(matches!(req, Request::Ping));

        send_frame(&mut a, &Response::Pong).unwrap();
        let resp: Response = recv_frame(&mut b).unwrap();
        assert!(matches!(resp, Response::Pong));
    }

    #[test]
    fn hello_version_roundtrip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        send_frame(
            &mut a,
            &Request::Hello {
                version: PROTO_VERSION,
            },
        )
        .unwrap();
        let req: Request = recv_frame(&mut b).unwrap();
        assert!(matches!(req, Request::Hello { version } if version == PROTO_VERSION));

        send_frame(
            &mut a,
            &Response::Hello {
                version: PROTO_VERSION,
            },
        )
        .unwrap();
        let resp: Response = recv_frame(&mut b).unwrap();
        assert!(matches!(resp, Response::Hello { version } if version == PROTO_VERSION));
    }

    #[test]
    fn list_payload_roundtrip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let infos = vec![SessionInfo {
            name: "kp-x-1".into(),
            status: "running".into(),
            engine: crate::engine::EngineKind::Native,
            ..Default::default()
        }];
        send_frame(&mut a, &Response::Sessions(infos.clone())).unwrap();
        let resp: Response = recv_frame(&mut b).unwrap();
        match resp {
            Response::Sessions(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].name, "kp-x-1");
                assert_eq!(v[0].engine, crate::engine::EngineKind::Native);
            }
            other => panic!("意外响应: {other:?}"),
        }
    }

    #[test]
    fn engine_kind_serde() {
        assert_eq!(
            serde_json::from_str::<EngineKind>("\"native\"").unwrap(),
            crate::engine::EngineKind::Native
        );
        assert_eq!(
            serde_json::from_str::<EngineKind>("\"tmux\"").unwrap(),
            crate::engine::EngineKind::Tmux
        );
    }
}
