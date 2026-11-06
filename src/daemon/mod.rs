mod reconcile;

use crate::engine::{Engine, EngineKind};
use crate::history::{self, HistoryEntry};
use crate::paths;
use crate::protocol::{
    recv_frame, send_frame, ExitInfo, Policy, Request, Response, SessionInfo, SessionSpec,
    SpawnOutcome, PROTO_VERSION,
};
use crate::registry::{Entry, EntryState, Registry, Status};
use crate::util::now_ms;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 进程退出到 kill-session 的宽限：给 attach 端留出收到 WaitExit 响应的时间窗（架构 §4.3）。
const EXIT_GRACE_MS: u64 = 2000;

pub struct Ctx {
    pub native: Box<dyn Engine>,
    pub tmux: Box<dyn Engine>,
    pub reg: Registry,
    pub exe: PathBuf,
    /// daemon 启动时刻（kpm doctor 自述用）
    pub started_at_ms: u64,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 策略粘性存储：会话名 → 生效策略（D11，policies.json 落盘）
    policies: Mutex<HashMap<String, Policy>>,
}

impl Ctx {
    /// 按会话引擎分派（D12：一个 daemon 同时管两种引擎的会话）。
    pub fn eng(&self, kind: EngineKind) -> &dyn Engine {
        match kind {
            EngineKind::Native => self.native.as_ref(),
            EngineKind::Tmux => self.tmux.as_ref(),
        }
    }

    fn session_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    /// 策略粘性解析：CLI -p > 已存储 > 配置解析值；生效即存储（FR3/D11）。
    fn resolve_policy(&self, name: &str, spec: &mut SessionSpec) -> Policy {
        let stored = self.policies.lock().unwrap().get(name).copied();
        let eff = spec.policy_cli.or(stored).unwrap_or(spec.policy);
        spec.policy = eff;
        spec.policy_cli = None;
        self.policies.lock().unwrap().insert(name.to_string(), eff);
        persist_policies(self);
        eff
    }
}

fn load_policies() -> HashMap<String, Policy> {
    paths::policies_file()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn persist_policies(ctx: &Ctx) {
    let Ok(p) = paths::policies_file() else {
        return;
    };
    let map = ctx.policies.lock().unwrap().clone();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("json.tmp");
    if let Ok(j) = serde_json::to_string_pretty(&map) {
        if std::fs::write(&tmp, j).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// daemon 生命周期日志（排障主线）：带 pid 与 ms 时间戳追加到 daemon.log，
/// holder 的 stderr 也重定向到同一文件，全程一条时间线。事件级低频，不分级别。
pub fn dlog(msg: &str) {
    let Ok(p) = paths::daemon_log() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        use std::io::Write;
        let _ = writeln!(f, "kp daemon[{}]: {} @{}", std::process::id(), msg, now_ms());
    }
}

pub fn serve() -> Result<(), String> {
    let rt = paths::runtime_dir().map_err(|e| e.to_string())?;
    paths::ensure_dir(&rt).map_err(|e| e.to_string())?;
    let sock_path = rt.join("daemon.sock");
    // 单实例锁：已有活 daemon 则静默退出（孵化方只会孵化一个）
    if sock_path.exists() && UnixStream::connect(&sock_path).is_ok() {
        return Err("已有 kp daemon 在运行".into());
    }
    let _ = std::fs::remove_file(&sock_path);
    paths::ensure_dir(&paths::logs_dir().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    paths::ensure_dir(&paths::specs_dir().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    let reg = Registry::load(paths::sessions_file().map_err(|e| e.to_string())?);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let ctx = Arc::new(Ctx {
        native: crate::engine::engine(EngineKind::Native),
        tmux: crate::engine::engine(EngineKind::Tmux),
        reg,
        exe,
        started_at_ms: now_ms(),
        locks: Mutex::new(HashMap::new()),
        policies: Mutex::new(load_policies()),
    });
    reconcile::run(&ctx);

    // unix socket 路径上限（macOS SUN_LEN=104）：提前给出可行动的报错
    if sock_path.to_string_lossy().len() > 100 {
        return Err(format!(
            "socket 路径过长（{}，上限 ~104）：请设置更短的 XDG_RUNTIME_DIR",
            sock_path.display()
        ));
    }
    let listener = UnixListener::bind(&sock_path).map_err(|e| format!("绑定 socket 失败: {e}"))?;
    {
        let c = ctx.clone();
        std::thread::spawn(move || janitor_loop(c));
    }
    println!("kp daemon: 就绪（{}）", sock_path.display());
    dlog(&format!(
        "就绪 v{}（{}）pid={} exe={}",
        env!("CARGO_PKG_VERSION"),
        sock_path.display(),
        std::process::id(),
        ctx.exe.display()
    ));
    for stream in listener.incoming().flatten() {
        let c = ctx.clone();
        std::thread::spawn(move || handle_conn(c, stream));
    }
    Ok(())
}

fn handle_conn(ctx: Arc<Ctx>, mut stream: UnixStream) {
    // 协议握手：首帧必须 Hello，版本不匹配直接拒绝（FR7）
    let first: Request = match recv_frame(&mut stream) {
        Ok(r) => r,
        Err(_) => return,
    };
    match first {
        Request::Hello { version } if version == PROTO_VERSION => {
            let _ = send_frame(
                &mut stream,
                &Response::Hello {
                    version: PROTO_VERSION,
                },
            );
        }
        Request::Hello { version } => {
            let _ = send_frame(
                &mut stream,
                &Response::Err {
                    code: 4,
                    message: format!(
                        "协议版本不匹配：client v{version} / daemon v{PROTO_VERSION}，请升级 kp"
                    ),
                },
            );
            return;
        }
        _ => {
            let _ = send_frame(
                &mut stream,
                &Response::Err {
                    code: 1,
                    message: "首帧必须是 Hello".into(),
                },
            );
            return;
        }
    }
    loop {
        let req: Request = match recv_frame(&mut stream) {
            Ok(r) => r,
            Err(_) => return,
        };
        let resp = dispatch(&ctx, req);
        if send_frame(&mut stream, &resp).is_err() {
            return;
        }
    }
}

fn dispatch(ctx: &Ctx, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Hello { .. } => Response::Err {
            code: 1,
            message: "重复握手".into(),
        },
        Request::Spawn { spec } => spawn(ctx, spec),
        Request::AttachExisting { name } => attach_existing(ctx, &name),
        Request::RestartByName { name, fresh_env } => restart_by_name(ctx, &name, fresh_env),
        Request::Stop { name } => stop_one(ctx, &name),
        Request::StopAll => stop_all(ctx),
        Request::List => Response::Sessions(list_sessions(ctx)),
        Request::Status { name } => match ctx.reg.get(&name) {
            Some(e) => Response::Detail(Some(build_info(ctx, &e, &name))),
            None => Response::Detail(None),
        },
        Request::WaitExit { name, timeout_ms } => wait_exit(ctx, &name, timeout_ms),
        Request::ReportDone { name, exit_code } => {
            finalize(ctx, &name, exit_code, "exited");
            Response::Ok
        }
        Request::Clean { history_keep } => clean(ctx, history_keep),
        Request::DaemonInfo => Response::DaemonInfo {
            pid: std::process::id() as i32,
            exe: ctx.exe.to_string_lossy().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_ms: ctx.started_at_ms,
        },
    }
}

// ---------- 会话构建 ----------

fn build_info(ctx: &Ctx, entry: &Arc<Entry>, name: &str) -> SessionInfo {
    let st = entry.st.lock().unwrap().clone();
    let mut info = SessionInfo {
        name: name.to_string(),
        argv: st.spec.argv.clone(),
        cwd: st.spec.cwd.to_string_lossy().to_string(),
        branch: st.spec.branch.clone(),
        policy: Some(st.spec.policy),
        status: match st.status {
            Status::Running => "running",
            Status::Exited => "exited",
        }
        .into(),
        started_at_ms: Some(st.started_at_ms),
        ended_at_ms: st.ended_at_ms,
        exit_code: st.exit_code,
        reason: st.reason.clone(),
        pane_pid: None,
        clients: 0,
        log_path: st.spec.log_path.to_string_lossy().to_string(),
        identity_hash: st.spec.identity_hash.clone(),
        engine: st.spec.engine,
        project_root: st
            .spec
            .project_root
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
    };
    if st.status == Status::Running {
        if let Ok(Some(i)) = ctx.eng(st.spec.engine).info(name) {
            info.pane_pid = Some(i.pane_pid);
            info.clients = i.attached;
        }
    }
    info
}

fn write_spec_file(spec: &SessionSpec) -> Result<(), String> {
    if let Some(p) = spec.spec_path.parent() {
        paths::ensure_dir(p).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(spec).map_err(|e| e.to_string())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600) // env 快照可能含密钥（架构 §9）
        .open(&spec.spec_path)
        .map_err(|e| format!("写 spec 失败: {e}"))?;
    f.write_all(&json).map_err(|e| e.to_string())
}

/// 新建会话：写 spec → **先注册** → 引擎 create（孵化）→ 观测项。
/// 注册先于孵化是幽灵 running 的根因修复：快速退出的会话其 ReportDone
/// 必须能命中已存在的条目，否则 finalize 静默丢弃、条目永停 Running。
fn create_session(ctx: &Ctx, spec: SessionSpec) -> Result<SessionInfo, String> {
    // 同名残留（注册表里没有的异常会话，任何引擎）先清掉
    for kind in [EngineKind::Native, EngineKind::Tmux] {
        if let Ok(Some(_)) = ctx.eng(kind).info(&spec.name) {
            ctx.eng(kind).kill_session(&spec.name)?;
        }
    }
    write_spec_file(&spec)?;
    if let Some(p) = spec.log_path.parent() {
        paths::ensure_dir(p).map_err(|e| e.to_string())?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_path)
        .map_err(|e| format!("创建日志文件失败: {e}"))?;

    let eng = ctx.eng(spec.engine);
    dlog(&format!("create {} engine={:?}", spec.name, spec.engine));
    // 先注册再孵化（幽灵 running 的根因修复）：瞬间退出的命令，holder 的
    // ReportDone 可能在 eng.create 返回前到达 daemon，finalize 查不到条目会
    // 静默丢弃 → 退出码丢失、条目永远停在 Running。注册先于孵化后，
    // ReportDone 必然晚于注册到达，竞态闭合。
    let name = spec.name.clone();
    ctx.reg.insert(
        &name,
        EntryState {
            spec: spec.clone(),
            started_at_ms: now_ms(),
            ended_at_ms: None,
            exit_code: None,
            reason: String::new(),
            status: Status::Running,
            pgid: None,
            history_done: false,
        },
    );
    ctx.reg.persist();
    if let Err(e) = eng.create(&spec, &ctx.exe) {
        dlog(&format!("create {} 失败: {e}", spec.name));
        ctx.reg.remove(&name);
        ctx.reg.persist();
        return Err(e);
    }
    let pgid = eng.info(&spec.name).ok().flatten().map(|i| i.pane_pid);
    if let Some(e) = ctx.reg.get(&name) {
        e.st.lock().unwrap().pgid = pgid;
    }
    if let Err(e) = eng.start_obs(&spec, &ctx.exe) {
        let _ = eng.kill_session(&spec.name);
        ctx.reg.remove(&name);
        ctx.reg.persist();
        return Err(format!("设置观测项失败: {e}"));
    }
    // 观测项就位 → 放行 _spawn（两阶段启动）
    let go = spec.spec_path.with_extension("json.go");
    let _ = std::fs::write(&go, b"");
    let e = ctx.reg.get(&name).expect("刚插入的会话");
    Ok(build_info(ctx, &e, &name))
}

// ---------- Spawn 与策略 ----------

fn spawn(ctx: &Ctx, mut spec: SessionSpec) -> Response {
    let name = spec.name.clone();
    let lock = ctx.session_lock(&name);
    let _g = lock.lock().unwrap();
    ctx.resolve_policy(&name, &mut spec);
    if let Some(entry) = ctx.reg.get(&name) {
        let st = entry.st.lock().unwrap().clone();
        if st.status == Status::Running {
            let info = build_info(ctx, &entry, &spec.name);
            return match spec.policy {
                Policy::Attach => {
                    let writable = info.clients == 0; // 多读单写：先到先得（架构 §4.2）
                    Response::Spawn(SpawnOutcome::Attached {
                        writable,
                        attach_args: ctx.eng(spec.engine).attach_args(&spec.name, !writable),
                        info,
                    })
                }
                Policy::Reject => Response::Spawn(SpawnOutcome::Rejected { info }),
                Policy::Wait => Response::Spawn(SpawnOutcome::Waiting { info }),
                Policy::New => return spawn_new_instance(ctx, spec),
                Policy::Restart => return do_restart(ctx, spec),
            };
        }
        // 已退出 → 清理后重建
        cleanup_dead(ctx, &spec.name);
    }
    let engine = spec.engine;
    match create_session(ctx, spec) {
        Ok(info) => Response::Spawn(SpawnOutcome::Started {
            writable: true,
            attach_args: ctx.eng(engine).attach_args(&info.name, false),
            info,
        }),
        Err(e) => Response::Err {
            code: 1,
            message: e,
        },
    }
}

/// 策略 new：原名后加序号并行新实例。
fn spawn_new_instance(ctx: &Ctx, mut spec: SessionSpec) -> Response {
    for n in 2..=64 {
        let cand = format!("{}-{n}", spec.name);
        if ctx.reg.get(&cand).is_some() {
            continue;
        }
        let lock = ctx.session_lock(&cand);
        let _g = lock.lock().unwrap();
        if ctx.reg.get(&cand).is_some() {
            continue;
        }
        spec.name = cand;
        let engine = spec.engine;
        return match create_session(ctx, spec) {
            Ok(info) => Response::Spawn(SpawnOutcome::Started {
                writable: true,
                attach_args: ctx.eng(engine).attach_args(&info.name, false),
                info,
            }),
            Err(e) => Response::Err {
                code: 1,
                message: e,
            },
        };
    }
    Response::Err {
        code: 1,
        message: "并行实例过多（>64）".into(),
    }
}

fn attach_existing(ctx: &Ctx, name: &str) -> Response {
    let Some(entry) = ctx.reg.get(name) else {
        return Response::Err {
            code: 1,
            message: format!("没有会话 {name}（kpm ls 查看）"),
        };
    };
    let st = entry.st.lock().unwrap().clone();
    if st.status != Status::Running {
        return Response::Err {
            code: 1,
            message: format!(
                "会话 {name} 已退出（退出码 {}，原因 {}）；重启: kpm restart {name}",
                st.exit_code.map(|c| c.to_string()).unwrap_or("?".into()),
                st.reason
            ),
        };
    }
    let info = build_info(ctx, &entry, name);
    let writable = info.clients == 0;
    Response::Attach(SpawnOutcome::Attached {
        writable,
        attach_args: ctx.eng(st.spec.engine).attach_args(name, !writable),
        info,
    })
}

fn restart_by_name(
    ctx: &Ctx,
    name: &str,
    fresh_env: Option<std::collections::BTreeMap<String, String>>,
) -> Response {
    let lock = ctx.session_lock(name);
    let _g = lock.lock().unwrap();
    let Some(entry) = ctx.reg.get(name) else {
        return Response::Err {
            code: 1,
            message: format!("没有会话 {name}"),
        };
    };
    let st = entry.st.lock().unwrap().clone();
    let mut spec = st.spec.clone();
    if let Some(env) = fresh_env {
        spec.env = env;
    }
    if st.status == Status::Running {
        do_restart(ctx, spec)
    } else {
        cleanup_dead(ctx, name);
        let engine = spec.engine;
        match create_session(ctx, spec) {
            Ok(info) => Response::Spawn(SpawnOutcome::Started {
                writable: true,
                attach_args: ctx.eng(engine).attach_args(&info.name, false),
                info,
            }),
            Err(e) => Response::Err {
                code: 1,
                message: e,
            },
        }
    }
}

/// restart：杀旧组 → 起新实例（同名，env 用快照 spec）→ attach（架构 §4.4）。
/// 调用方必须已持有该会话的 session_lock。
fn do_restart(ctx: &Ctx, spec: SessionSpec) -> Response {
    let name = spec.name.clone();
    let old_exit = kill_running(ctx, &name, "restart");
    if let Some(entry) = ctx.reg.get(&name) {
        append_history_once(&entry, None);
    }
    cleanup_dead(ctx, &name);
    let engine = spec.engine;
    match create_session(ctx, spec) {
        Ok(info) => Response::Spawn(SpawnOutcome::Restarted {
            writable: true,
            attach_args: ctx.eng(engine).attach_args(&name, false),
            old_exit,
            info,
        }),
        Err(e) => Response::Err {
            code: 1,
            message: e,
        },
    }
}

fn stop_one(ctx: &Ctx, name: &str) -> Response {
    let lock = ctx.session_lock(name);
    let _g = lock.lock().unwrap();
    let Some(entry) = ctx.reg.get(name) else {
        return Response::Err {
            code: 1,
            message: format!("没有会话 {name}"),
        };
    };
    let st = entry.st.lock().unwrap().clone();
    if st.status == Status::Running {
        kill_running(ctx, name, "stopped");
        let st2 = entry.st.lock().unwrap().clone();
        append_history_once(&entry, None);
        cleanup_dead(ctx, name);
        Response::Stopped(vec![(name.to_string(), st2.exit_code)])
    } else {
        cleanup_dead(ctx, name);
        Response::Stopped(vec![(name.to_string(), st.exit_code)])
    }
}

fn stop_all(ctx: &Ctx) -> Response {
    let mut out = Vec::new();
    for name in ctx.reg.names() {
        match stop_one(ctx, &name) {
            Response::Stopped(v) => out.extend(v),
            _ => out.push((name, None)),
        }
    }
    Response::Stopped(out)
}

// ---------- 杀灭 / 退出 / 清理 ----------

/// 杀整组：TERM → term_timeout → KILL（FR6）。返回退出码（remain-on-exit 可读）。
fn kill_running(ctx: &Ctx, name: &str, reason: &str) -> Option<i32> {
    dlog(&format!("kill {name} reason={reason}"));
    let entry = ctx.reg.get(name)?;
    let st = entry.st.lock().unwrap().clone();
    if st.status != Status::Running {
        return None;
    }
    let pgid = match st.pgid {
        Some(p) => p,
        None => ctx
            .eng(st.spec.engine)
            .info(name)
            .ok()
            .flatten()
            .map(|i| i.pane_pid)
            .filter(|p| *p != 0)?,
    };
    // 预标记原因：_spawn 的自报（finalize，reason=exited）可能与杀灭竞态，
    // 先写入 restart/stopped 才能让 WaitExit 的跟随逻辑与历史记录不真相被覆盖。
    {
        let mut g = entry.st.lock().unwrap();
        g.reason = reason.to_string();
    }
    // 组杀失败（组 leader 已变）时退化为单进程杀
    let eng = ctx.eng(st.spec.engine);
    if eng.kill_group(pgid, libc::SIGTERM).is_err() {
        let _ = unsafe { libc::kill(pgid, libc::SIGTERM) };
    }
    if !wait_dead(ctx, name, st.spec.term_timeout) {
        let _ = eng.kill_group(pgid, libc::SIGKILL);
        let _ = unsafe { libc::kill(pgid, libc::SIGKILL) };
        wait_dead(ctx, name, 3);
    }
    let code = ctx
        .eng(st.spec.engine)
        .info(name)
        .ok()
        .flatten()
        .and_then(|i| i.pane_dead_status);
    {
        let mut g = entry.st.lock().unwrap();
        if g.status == Status::Running {
            g.status = Status::Exited;
            g.exit_code = code;
            g.ended_at_ms = Some(now_ms());
            g.reason = reason.to_string();
        }
    }
    entry.cv.notify_all();
    code
}

fn wait_dead(ctx: &Ctx, name: &str, timeout_s: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    let kind = ctx.reg.get(name).map(|e| e.st.lock().unwrap().spec.engine);
    let Some(kind) = kind else { return true };
    loop {
        match ctx.eng(kind).info(name) {
            Ok(None) => return true,
            Ok(Some(i)) if i.pane_dead => return true,
            _ => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 退出入账：状态、历史、唤醒 WaitExit（hook 与 janitor 双路径共用，幂等）。
fn finalize(ctx: &Ctx, name: &str, code: Option<i32>, reason: &str) {
    let Some(entry) = ctx.reg.get(name) else {
        // 条目不存在：幽灵 running 的症状镜像（ReportDone 早于注册）。正常不应发生。
        dlog(&format!("finalize {name} code={code:?} reason={reason} —— 条目不存在，丢弃"));
        return;
    };
    {
        let mut g = entry.st.lock().unwrap();
        if g.status != Status::Running {
            return; // hook 与 janitor 竞争时后者让路
        }
        g.status = Status::Exited;
        g.exit_code = code;
        g.ended_at_ms = Some(now_ms());
        if g.reason.is_empty() {
            // 杀灭路径已预标记 restart/stopped，不可覆盖
            g.reason = reason.to_string();
        }
    }
    dlog(&format!("finalize {name} code={code:?} reason={reason}"));
    entry.cv.notify_all();
    append_history_once(&entry, None);
    ctx.reg.persist();
}

/// 移除注册条目 + 清引擎侧残留 + 删 spec。
fn cleanup_dead(ctx: &Ctx, name: &str) {
    dlog(&format!("cleanup {name}"));
    let kind = ctx.reg.get(name).map(|e| e.st.lock().unwrap().spec.engine);
    ctx.reg.remove(name);
    match kind {
        Some(k) => {
            let _ = ctx.eng(k).kill_session(name);
        }
        None => {
            // 条目不存在（极端残留）：两个引擎都清一遍
            let _ = ctx.eng(EngineKind::Native).kill_session(name);
            let _ = ctx.eng(EngineKind::Tmux).kill_session(name);
        }
    }
    if let Ok(d) = paths::specs_dir() {
        let _ = std::fs::remove_file(d.join(format!("{name}.json")));
        let _ = std::fs::remove_file(d.join(format!("{name}.json.go")));
    }
    ctx.reg.persist();
}

/// 历史只入账一次：_spawn 主动上报（finalize）、stop/restart、hook 多路径竞态共用。
fn append_history_once(entry: &Arc<Entry>, reason: Option<&str>) {
    let (state, reason_final) = {
        let mut g = entry.st.lock().unwrap();
        if g.history_done {
            return;
        }
        g.history_done = true;
        let r = reason
            .map(|s| s.to_string())
            .unwrap_or_else(|| g.reason.clone());
        (g.clone(), r)
    };
    let Ok(p) = paths::history_file() else {
        return;
    };
    let e = HistoryEntry {
        name: state.spec.name.clone(),
        argv: state.spec.argv.clone(),
        cwd: state.spec.cwd.to_string_lossy().to_string(),
        branch: state.spec.branch.clone(),
        policy: state.spec.policy,
        started_at_ms: state.started_at_ms,
        ended_at_ms: state.ended_at_ms.unwrap_or_else(now_ms),
        exit_code: state.exit_code,
        reason: reason_final,
    };
    let _ = history::append(&p, &e);
}

// ---------- 查询类 ----------

fn list_sessions(ctx: &Ctx) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for name in ctx.reg.names() {
        if let Some(e) = ctx.reg.get(&name) {
            out.push(build_info(ctx, &e, &name));
        }
    }
    out
}

fn wait_exit(ctx: &Ctx, name: &str, timeout_ms: u64) -> Response {
    let Some(mut entry) = ctx.reg.get(name) else {
        return Response::Err {
            code: 1,
            message: format!("没有会话 {name}"),
        };
    };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let snapshot = entry.st.lock().unwrap().clone();
        if snapshot.status == Status::Exited {
            if snapshot.reason == "restart" {
                // 会话因 restart 退出：跟随同名新实例继续等待（客户端 attach 跟随的前提）
                std::thread::sleep(Duration::from_millis(100));
                if let Some(e2) = ctx.reg.get(name) {
                    entry = e2;
                }
                continue;
            }
            let dur = snapshot
                .ended_at_ms
                .unwrap_or_else(now_ms)
                .saturating_sub(snapshot.started_at_ms);
            return Response::Exit(Some(ExitInfo {
                exit_code: snapshot.exit_code,
                duration_ms: dur,
                reason: snapshot.reason.clone(),
            }));
        }
        let now = Instant::now();
        if now >= deadline {
            return Response::Exit(None);
        }
        let guard = entry.st.lock().unwrap();
        if guard.status != Status::Running {
            continue; // 状态已变，回到顶部快照判断
        }
        let _ = entry.cv.wait_timeout(guard, deadline - now).unwrap();
    }
}

fn clean(ctx: &Ctx, history_keep: u32) -> Response {
    for (name, st) in ctx.reg.snapshot() {
        if st.status == Status::Exited {
            let _ = ctx.eng(st.spec.engine).kill_session(&name);
            ctx.reg.remove(&name);
            let _ = std::fs::remove_file(&st.spec.spec_path);
            let _ = std::fs::remove_file(st.spec.spec_path.with_extension("json.go"));
        }
    }
    if let Ok(p) = paths::history_file() {
        let _ = history::prune(&p, history_keep as usize);
    }
    ctx.reg.persist();
    Response::Cleaned
}

// ---------- janitor ----------

fn janitor_loop(ctx: Arc<Ctx>) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        // 收割孵化产物（holder）的僵尸
        unsafe {
            let mut st = 0 as libc::c_int;
            while libc::waitpid(-1, &mut st, libc::WNOHANG) > 0 {}
        }
        for (name, st) in ctx.reg.snapshot() {
            let eng = ctx.eng(st.spec.engine);
            match st.status {
                Status::Running => match eng.info(&name) {
                    // 注册先于孵化（见 create_session）：启动窗口内引擎状态尚未就绪，
                    // 不误判 session-gone；超过 2s 仍查无实况才视为 holder 崩溃/引擎残留丢失
                    Ok(None) if now_ms().saturating_sub(st.started_at_ms) > 2000 => {
                        finalize(&ctx, &name, None, "session-gone")
                    }
                    Ok(None) => {}
                    Ok(Some(i)) if i.pane_dead => {
                        finalize(&ctx, &name, i.pane_dead_status, "exited")
                    }
                    _ => {
                        if st.spec.engine == EngineKind::Tmux {
                            cap_log(&ctx, &st); // native 的日志限额由 holder 自管
                        }
                    }
                },
                Status::Exited => {
                    if let Some(ended) = st.ended_at_ms {
                        if now_ms() >= ended + EXIT_GRACE_MS {
                            dlog(&format!("janitor 清尾 {name}（exited 已超宽限）"));
                            let _ = eng.kill_session(&name);
                        }
                    }
                }
            }
        }
    }
}

/// 日志限额：超限即停 pipe → 截断 → 重开（FR8；轮转 M2）。仅 tmux 引擎路径。
fn cap_log(ctx: &Ctx, st: &EntryState) {
    let max = st.spec.log_max_mb.saturating_mul(1024 * 1024);
    if max == 0 {
        return;
    }
    if let Ok(md) = std::fs::metadata(&st.spec.log_path) {
        if md.len() > max {
            let _ = ctx
                .eng(st.spec.engine)
                .pipe(&st.spec.name, &st.spec.log_path, false);
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&st.spec.log_path);
            let _ = ctx
                .eng(st.spec.engine)
                .pipe(&st.spec.name, &st.spec.log_path, true);
        }
    }
}
