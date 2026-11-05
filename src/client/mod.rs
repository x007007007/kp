use crate::config;
use crate::history::{self, HistoryEntry};
use crate::identity;
use crate::paths;
use crate::protocol::{
    recv_frame, send_frame, ExitInfo, Policy, Request, Response, SessionSpec, SpawnOutcome,
    PROTO_VERSION,
};
use crate::util::{fmt_duration, now_ms, slug};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub type RpcError = (i32, String);

fn die(code: i32, msg: &str) -> i32 {
    eprintln!("kp: {msg}");
    code
}

/// 内部命令清单（与 kp.rs 的 INTERNAL_CMDS 对应）；被包装会递归拉会话 → fork bomb。
fn kp_cli_internal(a: &str) -> bool {
    matches!(a, "_spawn" | "_report" | "_holder" | "_attach")
}

/// daemon 孵化前的引擎选择（[run].engine；daemon 启动时读取 KP_ENGINE）。
fn apply_engine_env() {
    if std::env::var_os("KP_ENGINE").is_some() {
        return;
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(cfg) = config::load(&cwd, None) {
            let label = match cfg.run.engine {
                crate::engine::EngineKind::Native => "native",
                crate::engine::EngineKind::Tmux => "tmux",
            };
            std::env::set_var("KP_ENGINE", label);
        }
    }
}

// ---------- RPC ----------

fn connect_daemon() -> Result<UnixStream, RpcError> {
    apply_engine_env();
    let sock = paths::socket_path().map_err(|e| (1, e.to_string()))?;
    for attempt in 0..30u32 {
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
        if attempt == 0 {
            incubate().map_err(|e| (1, e))?;
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err((1, format!("无法连接 kp daemon（{}）", sock.display())))
}

/// 首次调用孵化 daemon（ssh-agent 模式，FR7）：detached + 输出重定向到 daemon.log。
/// 注意：kpm 触发孵化时 current_exe 是 kpm，daemon 只能从 kp 二进制孵化。
fn incubate() -> Result<(), String> {
    let mut exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if exe.file_name().map(|n| n == "kpm").unwrap_or(false) {
        exe.set_file_name("kp");
    }
    if !exe.is_file() {
        return Err(format!(
            "找不到 kp 二进制（{}）：kpm 与 kp 需安装在同一目录",
            exe.display()
        ));
    }
    let log = paths::daemon_log().map_err(|e| e.to_string())?;
    if let Some(p) = log.parent() {
        let _ = paths::ensure_dir(p);
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| e.to_string())?;
    let f2 = f.try_clone().map_err(|e| e.to_string())?;
    Command::new(exe)
        .arg("--daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(f2))
        .stderr(std::process::Stdio::from(f))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("孵化 daemon 失败: {e}"))?;
    Ok(())
}

fn rpc(req: Request) -> Result<Response, RpcError> {
    // 版本不匹配（二进制刚升级、daemon 还是旧版）→ 自动重启 daemon 后重试一次。
    // 会话由 tmux 承载不受影响，新 daemon 启动时 reconcile 接管（架构 §4.5）。
    match rpc_once(req.clone()) {
        Err((4, _)) => {
            force_daemon_restart();
            rpc_once(req)
        }
        other => other,
    }
}

/// 杀掉旧版 daemon 并重新孵化（同机同二进制路径，版本错配只在升级后出现）。
fn force_daemon_restart() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new("pkill")
            .args(["-f", &format!("{} --daemon", exe.display())])
            .status();
    }
    if let Ok(p) = paths::socket_path() {
        let _ = std::fs::remove_file(p);
    }
    thread::sleep(Duration::from_millis(300));
    let _ = incubate();
    thread::sleep(Duration::from_millis(300));
}

fn rpc_once(req: Request) -> Result<Response, RpcError> {
    let mut s = connect_daemon()?;
    send_frame(
        &mut s,
        &Request::Hello {
            version: PROTO_VERSION,
        },
    )
    .map_err(|e| (1, e.to_string()))?;
    match recv_frame(&mut s).map_err(|e| (1, e.to_string()))? {
        Response::Hello { version } if version == PROTO_VERSION => {}
        Response::Hello { version } => {
            return Err((
                4,
                format!("协议版本不匹配（client v{PROTO_VERSION} / daemon v{version}），请升级 kp"),
            ))
        }
        // 旧版 daemon 对新版本 Hello 的拒绝形态是 Err{code:4}
        Response::Err { code: 4, message } => return Err((4, message)),
        _ => return Err((1, "daemon 握手异常".into())),
    }
    send_frame(&mut s, &req).map_err(|e| (1, e.to_string()))?;
    recv_frame(&mut s).map_err(|e| (1, e.to_string()))
}

// ---------- kp CMD... ----------

pub fn cmd_run(name_opt: Option<String>, policy_opt: Option<Policy>, argv: Vec<String>) -> i32 {
    if argv.is_empty() {
        return die(1, "没有要运行的命令");
    }
    // 防线：内部子命令绝不允许被当作被包装命令（否则会话 pane 递归拉起会话 → fork bomb）
    let is_kp_self = argv[0] == "kp" || argv[0].ends_with("/kp");
    if argv.len() >= 2 && is_kp_self && kp_cli_internal(argv[1].as_str()) {
        return die(1, &format!("内部命令 {:?} 不可被包装", argv[1]));
    }
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => return die(1, &format!("无法获取当前目录: {e}")),
    };
    let cfg = match config::load(&cwd, None) {
        Ok(c) => c,
        Err(e) => return die(2, &e),
    };
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let anchor = cfg.project_root.clone().unwrap_or_else(|| cwd.clone());
    let proj_name = anchor
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "proj".into());
    let hash = identity::hash16(&argv, &anchor, &env, &cfg.ignore_env);
    let argv_str = argv.join(" ");

    let (name, policy) = if let Some(n) = name_opt {
        if !config::valid_name(&n) {
            return die(
                2,
                &format!("非法会话名: {n:?}（^[A-Za-z0-9][A-Za-z0-9_-]{{0,63}}$）"),
            );
        }
        (
            format!("kp-{}-{}", slug(&proj_name, 24), slug(&n, 32)),
            policy_opt.unwrap_or(cfg.run.default_policy),
        )
    } else if let Some(s) = config::resolve_service(&cfg, &argv_str, &cwd) {
        // 命中 service：name 即身份（与 -n 等价），加项目前缀防跨项目冲突
        (
            format!("kp-{}-{}", slug(&proj_name, 24), s.name),
            policy_opt.or(s.policy).unwrap_or(cfg.run.default_policy),
        )
    } else {
        (
            identity::session_name(&proj_name, &hash),
            policy_opt.unwrap_or(cfg.run.default_policy),
        )
    };
    let (log_path, spec_path) = match (paths::logs_dir(), paths::specs_dir()) {
        (Ok(l), Ok(s)) => (
            l.join(format!("{name}.log")),
            s.join(format!("{name}.json")),
        ),
        (Err(e), _) | (_, Err(e)) => return die(1, &format!("状态目录不可用: {e}")),
    };
    let spec = SessionSpec {
        name: name.clone(),
        argv,
        cwd: cwd.clone(),
        env,
        policy,
        policy_cli: policy_opt,
        engine: cfg.run.engine,
        log_path,
        spec_path,
        branch: identity::git_info(cfg.project_root.as_deref()),
        project_root: cfg.project_root.clone(),
        identity_hash: hash,
        term_timeout: cfg.run.term_timeout,
        log_max_mb: cfg.run.log_max_mb,
    };

    let mut wait_printed = false;
    loop {
        match rpc(Request::Spawn { spec: spec.clone() }) {
            Ok(Response::Spawn(SpawnOutcome::Waiting { info })) => {
                if !wait_printed {
                    eprintln!("⏳ kp: 等待现有会话 {} 退出（策略 wait）…", info.name);
                    wait_printed = true;
                }
                if let Err(e) = wait_exit_forever(&info.name) {
                    return die(1, &e);
                }
            }
            Ok(Response::Spawn(outcome)) => return handle_outcome(&outcome),
            Ok(Response::Err { code, message }) => return die(code, &message),
            Ok(_) => return die(1, "daemon 响应异常"),
            Err((c, m)) => return die(c, &m),
        }
    }
}

fn wait_exit_forever(name: &str) -> Result<(), String> {
    loop {
        match rpc(Request::WaitExit {
            name: name.to_string(),
            timeout_ms: 3_600_000,
        }) {
            Ok(Response::Exit(Some(ei))) if ei.reason == "restart" => continue, // 重启中，继续等新实例
            Ok(Response::Exit(Some(_))) => return Ok(()),
            Ok(Response::Exit(None)) => continue, // 长轮询续期
            Ok(Response::Err { message, .. }) => return Err(message),
            Ok(_) => return Err("daemon 响应异常".into()),
            Err((_, m)) => return Err(m),
        }
    }
}

fn handle_outcome(outcome: &SpawnOutcome) -> i32 {
    match outcome {
        SpawnOutcome::Rejected { info } => {
            eprintln!(
                "✗ kp: 会话 {} 已在运行，当前命令被策略 reject 拒绝",
                info.name
            );
            eprintln!("  接入: kpm attach {}", info.name);
            eprintln!("  查看: kpm ls");
            3
        }
        SpawnOutcome::Started {
            info, attach_args, ..
        } => {
            banner("已启动", info);
            attach_session(&info.name, attach_args)
        }
        SpawnOutcome::Attached {
            info,
            attach_args,
            writable,
        } => {
            banner(
                if *writable {
                    "已接入（可写）"
                } else {
                    "已接入（只读 · 多读单写）"
                },
                info,
            );
            attach_session(&info.name, attach_args)
        }
        SpawnOutcome::Restarted {
            info,
            attach_args,
            old_exit,
            ..
        } => {
            if let Some(c) = old_exit {
                eprintln!("kp: 旧实例已停止（退出码 {c}）");
            }
            banner("已重启", info);
            attach_session(&info.name, attach_args)
        }
        SpawnOutcome::Waiting { .. } => unreachable!("Waiting 在 cmd_run 中处理"),
    }
}

fn banner(mode: &str, info: &crate::protocol::SessionInfo) {
    eprintln!("─ kp ──────────────────────────────────────────");
    eprintln!(" 会话   {}（{mode}）", info.name);
    eprintln!(" 命令   {}", info.argv.join(" "));
    eprintln!(" 目录   {}   分支 {}", info.cwd, info.branch);
    if let (Some(pid), Some(t0)) = (info.pane_pid, info.started_at_ms) {
        eprintln!(
            " PID    {pid}   已运行 {}",
            fmt_duration(now_ms().saturating_sub(t0))
        );
    }
    if let Some(p) = info.policy {
        eprintln!(" 策略   {:?}（重复时生效；-p 可临时覆盖并改设粘性策略）", p);
    }
    eprintln!(" 退出   任一窗口 Ctrl-C = 结束会话并退出全部窗口；Ctrl-b d = 仅脱离（Ctrl-b Ctrl-b 发送字面 Ctrl-b）");
    eprintln!("───────────────────────────────────────────────");
}

/// attach 管控结构（架构 A3）：fork tmux attach 承担终端 I/O，自身阻塞 WaitExit 长轮询。
/// restart 场景（本窗口或其他窗口发起）：旧实例退出后按"实例纪元"（started_at_ms）
/// 自动跟随接入同名新实例（FR3），而不是被甩出。
fn attach_session(name: &str, attach_args: &[String]) -> i32 {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    let mut base_started: Option<u64> = None;
    if let Ok(Response::Detail(Some(i))) = rpc(Request::Status {
        name: name.to_string(),
    }) {
        base_started = i.started_at_ms;
    }
    loop {
        let (tx, rx) = mpsc::channel::<Result<ExitInfo, String>>();
        {
            let name = name.to_string();
            thread::spawn(move || loop {
                match rpc(Request::WaitExit {
                    name: name.clone(),
                    timeout_ms: 3_600_000,
                }) {
                    Ok(Response::Exit(Some(ei))) => {
                        let _ = tx.send(Ok(ei));
                        return;
                    }
                    Ok(Response::Exit(None)) => continue,
                    Ok(Response::Err { message, .. }) => {
                        let _ = tx.send(Err(message));
                        return;
                    }
                    Ok(_) => {
                        let _ = tx.send(Err("daemon 响应异常".into()));
                        return;
                    }
                    Err((_, m)) => {
                        let _ = tx.send(Err(m));
                        return;
                    }
                }
            });
        }
        let mut cmd = Command::new(&attach_args[0]);
        cmd.args(&attach_args[1..])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        let attach_status = match cmd.status() {
            Ok(s) => s,
            Err(e) => {
                return die(
                    1,
                    &format!(
                        "无法启动 tmux attach（需要 tmux ≥ 3.0，macOS: brew install tmux）: {e}"
                    ),
                )
            }
        };
        // 快速路径：WaitExit 已回（含退出原因）
        if let Ok(Ok(ei)) = rx.recv_timeout(Duration::from_secs(3)) {
            if ei.reason == "restart" && wait_follow(name, &mut base_started) {
                eprintln!("kp: 服务已重启，已重新接入新实例");
                continue;
            }
            return print_exit_summary(&ei);
        }
        // attach 先于 WaitExit 返回：可能是用户 detach，也可能是 restart 进行中。
        // 以实例纪元判定：running 且 started_at_ms 变了 → 跟随；相同 → 用户 detach。
        let mut followed = false;
        let mut session_ended = false;
        for _ in 0..60 {
            thread::sleep(Duration::from_millis(250));
            match rpc(Request::Status {
                name: name.to_string(),
            }) {
                Ok(Response::Detail(Some(i))) if i.status == "running" => {
                    if base_started != i.started_at_ms {
                        followed = true;
                        base_started = i.started_at_ms;
                        break;
                    }
                    eprintln!("\nkp: 已脱离会话 {name}（会话继续运行；恢复: kpm attach {name}）");
                    return 0;
                }
                Ok(Response::Detail(Some(i))) if i.status == "exited" && i.reason != "restart" => {
                    session_ended = true;
                    break;
                    // restart 进行中则继续等新实例
                }
                _ => {} // 会话重建中
            }
        }
        if followed {
            eprintln!("kp: 服务已重启，已重新接入新实例");
            continue;
        }
        if session_ended {
            if let Ok(Ok(ei)) = rx.recv_timeout(Duration::from_secs(5)) {
                return print_exit_summary(&ei);
            }
            eprintln!("\nkp: 会话已结束（详见 kpm history）");
            return if attach_status.success() {
                0
            } else {
                attach_status.code().unwrap_or(1)
            };
        }
        eprintln!("\nkp: 已脱离会话 {name}（会话继续运行；恢复: kpm attach {name}）");
        return 0;
    }
}

/// restart 跟随：轮询等待新实例出现（至多 ~15s）。出现即更新纪元并返回 true。
fn wait_follow(name: &str, base: &mut Option<u64>) -> bool {
    for _ in 0..60 {
        thread::sleep(Duration::from_millis(250));
        match rpc(Request::Status {
            name: name.to_string(),
        }) {
            Ok(Response::Detail(Some(i))) if i.status == "running" => {
                if *base != i.started_at_ms {
                    *base = i.started_at_ms;
                    return true;
                }
                return false;
            }
            Ok(Response::Detail(Some(i))) if i.status == "exited" && i.reason != "restart" => {
                return false
            }
            _ => {}
        }
    }
    false
}

fn print_exit_summary(ei: &ExitInfo) -> i32 {
    eprintln!(
        "\nkp: 会话退出 · 退出码 {} · 用时 {}",
        ei.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into()),
        fmt_duration(ei.duration_ms)
    );
    ei.exit_code.unwrap_or(0)
}

// ---------- 管理子命令（kpm 入口） ----------

pub fn cmd_ls() -> i32 {
    match rpc(Request::List) {
        Ok(Response::Sessions(list)) => {
            if list.is_empty() {
                println!("没有会话");
                return 0;
            }
            let w = list
                .iter()
                .map(|i| i.name.len())
                .max()
                .unwrap_or(4)
                .clamp(4, 60);
            let (h_name, h_status, h_policy, h_pid, h_age, h_cli, h_branch, h_cmd) = (
                "NAME", "STATUS", "POLICY", "PID", "AGE", "CLI", "BRANCH", "CMD",
            );
            println!(
                "{h_name:<w$} {h_status:<8} {h_policy:<8} {h_pid:<7} {h_age:>8} {h_cli:>4}  {h_branch:<12} {h_cmd}"
            );
            for i in &list {
                let age = i
                    .started_at_ms
                    .map(|t| fmt_duration(now_ms().saturating_sub(t)))
                    .unwrap_or_else(|| "-".into());
                let pid = i
                    .pane_pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into());
                let status = if i.status == "exited" {
                    format!(
                        "exit={}",
                        i.exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "?".into())
                    )
                } else {
                    i.status.clone()
                };
                let policy = i
                    .policy
                    .map(|p| format!("{p:?}").to_lowercase())
                    .unwrap_or_else(|| "-".into());
                let eng = format!("{:?}", i.engine).to_lowercase();
                println!(
                    "{:<w$} {:<8} {:<8} {:<7} {:<7} {:>8} {:>4}  {:<12} {}",
                    i.name,
                    status,
                    policy,
                    eng,
                    pid,
                    age,
                    i.clients,
                    i.branch,
                    i.argv.join(" ")
                );
            }
            0
        }
        Ok(Response::Err { code, message }) => die(code, &message),
        Ok(_) => die(1, "daemon 响应异常"),
        Err((c, m)) => die(c, &m),
    }
}

pub fn cmd_status(name: &str) -> i32 {
    match rpc(Request::Status {
        name: name.to_string(),
    }) {
        Ok(Response::Detail(Some(i))) => {
            println!("名称     {}", i.name);
            println!("状态     {}", i.status);
            println!("命令     {}", i.argv.join(" "));
            println!("工作目录 {}", i.cwd);
            if !i.project_root.is_empty() {
                println!("项目根   {}", i.project_root);
            }
            println!("分支     {}", i.branch);
            if let Some(p) = i.policy {
                println!("策略     {p:?}（粘性；-p 可改设）");
            }
            if let Some(pid) = i.pane_pid {
                println!("PID      {pid}（客户端 {} 个）", i.clients);
            }
            if let Some(t) = i.started_at_ms {
                print!("开始于   {} 前", fmt_duration(now_ms().saturating_sub(t)));
                if let Some(e) = i.ended_at_ms {
                    println!("，结束于 {} 前", fmt_duration(now_ms().saturating_sub(e)));
                } else {
                    println!();
                }
            }
            if i.status == "exited" {
                println!(
                    "退出     {}（原因: {}）",
                    i.exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                    i.reason
                );
            }
            println!("日志     {}", i.log_path);
            if !i.identity_hash.is_empty() {
                println!("身份     {}", i.identity_hash);
            }
            0
        }
        Ok(Response::Detail(None)) => die(1, &format!("没有会话 {name}")),
        Ok(Response::Err { code, message }) => die(code, &message),
        Ok(_) => die(1, "daemon 响应异常"),
        Err((c, m)) => die(c, &m),
    }
}

pub fn cmd_attach(name: &str) -> i32 {
    match rpc(Request::AttachExisting {
        name: name.to_string(),
    }) {
        Ok(Response::Attach(outcome)) => handle_outcome(&outcome),
        Ok(Response::Err { code, message }) => die(code, &message),
        Ok(_) => die(1, "daemon 响应异常"),
        Err((c, m)) => die(c, &m),
    }
}

pub fn cmd_restart(name: &str, fresh_env: bool) -> i32 {
    let fresh = if fresh_env {
        Some(std::env::vars().collect::<BTreeMap<_, _>>())
    } else {
        None
    };
    match rpc(Request::RestartByName {
        name: name.to_string(),
        fresh_env: fresh,
    }) {
        Ok(Response::Spawn(outcome)) => handle_outcome(&outcome),
        Ok(Response::Err { code, message }) => die(code, &message),
        Ok(_) => die(1, "daemon 响应异常"),
        Err((c, m)) => die(c, &m),
    }
}

pub fn cmd_stop(name: Option<String>, all: bool) -> i32 {
    let req = if all {
        Request::StopAll
    } else {
        let Some(n) = name else {
            return die(1, "用法: kpm stop <name> 或 kpm stop --all");
        };
        Request::Stop { name: n }
    };
    match rpc(req) {
        Ok(Response::Stopped(list)) => {
            for (n, code) in list {
                println!(
                    "✓ 已停止 {n}（退出码 {}）",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                );
            }
            0
        }
        Ok(Response::Err { code, message }) => die(code, &message),
        Ok(_) => die(1, "daemon 响应异常"),
        Err((c, m)) => die(c, &m),
    }
}

pub fn cmd_logs(name: &str, follow: bool, lines: usize) -> i32 {
    let path = match rpc(Request::Status {
        name: name.to_string(),
    }) {
        Ok(Response::Detail(Some(i))) => PathBuf::from(i.log_path),
        Ok(Response::Detail(None)) => return die(1, &format!("没有会话 {name}")),
        Ok(Response::Err { code, message }) => return die(code, &message),
        Ok(_) => return die(1, "daemon 响应异常"),
        Err((c, m)) => return die(c, &m),
    };
    for l in tail_lines(&path, lines) {
        println!("{l}");
    }
    if !follow {
        return 0;
    }
    let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut ticks = 0u32;
    loop {
        thread::sleep(Duration::from_millis(200));
        ticks += 1;
        if let Ok(mut f) = std::fs::File::open(&path) {
            if f.seek(SeekFrom::Start(offset)).is_ok() {
                let mut buf = String::new();
                if f.read_to_string(&mut buf).is_ok() && !buf.is_empty() {
                    offset += buf.len() as u64;
                    print!("{buf}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
        // 每 1s 检查会话状态：已结束且无新数据则收场
        if ticks.is_multiple_of(5) {
            match rpc(Request::Status {
                name: name.to_string(),
            }) {
                Ok(Response::Detail(Some(i))) if i.status == "running" => {}
                _ => break,
            }
        }
    }
    0
}

fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(64 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = buf.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // 可能是半截行
    }
    let take = n.min(lines.len());
    lines[lines.len() - take..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub fn cmd_history(limit: usize) -> i32 {
    let path = match paths::history_file() {
        Ok(p) => p,
        Err(e) => return die(1, &e.to_string()),
    };
    let mut all = history::read(&path);
    all.reverse();
    if all.is_empty() {
        println!("没有历史记录");
        return 0;
    }
    let (h_time, h_name, h_exit, h_dur, h_reason, h_cmd) =
        ("TIME", "NAME", "EXIT", "DURATION", "REASON", "CMD");
    println!("{h_time:<10} {h_name:<10} {h_exit:>6} {h_dur:>8}  {h_reason:<14} {h_cmd}");
    for e in all.into_iter().take(limit) {
        let ago = fmt_duration(now_ms().saturating_sub(e.ended_at_ms));
        println!(
            "{:<10} {:<10} {:>6} {:>8}  {:<14} {}",
            format!("{ago}前"),
            e.name,
            e.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            fmt_duration(e.ended_at_ms.saturating_sub(e.started_at_ms)),
            e.reason,
            e.argv.join(" ")
        );
    }
    0
}

pub fn cmd_clean() -> i32 {
    let cfg = match std::env::current_dir()
        .map_err(|e| e.to_string())
        .and_then(|cwd| config::load(&cwd, None))
    {
        Ok(c) => c,
        Err(e) => return die(2, &e),
    };
    match rpc(Request::Clean {
        history_keep: cfg.run.history_keep,
    }) {
        Ok(Response::Cleaned) => {
            println!("✓ 已清理已退出会话、spec 与过期历史");
            0
        }
        Ok(Response::Err { code, message }) => die(code, &message),
        Ok(_) => die(1, "daemon 响应异常"),
        Err((c, m)) => die(c, &m),
    }
}

pub fn cmd_config() -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => return die(1, &e.to_string()),
    };
    let cfg = match config::load(&cwd, None) {
        Ok(c) => c,
        Err(e) => return die(2, &e),
    };
    println!(
        "项目根     {}",
        match (&cfg.project_root, &cfg.project_file) {
            (Some(root), Some(f)) => format!("{}（发现 {}）", root.display(), f.display()),
            _ => "未发现（无 .kp.toml，身份退回调用目录）".to_string(),
        }
    );
    println!(
        "全局配置   {}",
        cfg.global_file
            .as_ref()
            .map(|p| format!(
                "{}（{}）",
                p.display(),
                if p.is_file() { "存在" } else { "不存在" }
            ))
            .unwrap_or_else(|| "默认路径解析失败".into())
    );
    let r = &cfg.run;
    println!(
        "[run]      default_policy={:?} term_timeout={}s log_max_mb={}MB history_keep={} log_keep={} notify={}",
        r.default_policy, r.term_timeout, r.log_max_mb, r.history_keep, r.log_keep, r.notify
    );
    println!(
        "[identity] ignore_env = {:?}（追加于内置默认表之上）",
        cfg.ignore_env
    );
    if cfg.services.is_empty() {
        println!("[[services]] 无声明");
        return 0;
    }
    println!("[[services]] 候选链（项目层优先，首个 match 命中生效）：");
    for (idx, s) in cfg.services.iter().enumerate() {
        let layer = if idx < cfg.project_services {
            "项目"
        } else {
            "全局"
        };
        println!(
            "  {:>2}. [{layer}] name={:?} match={:?} cwd={:?} policy={}",
            idx + 1,
            s.name,
            s.match_,
            if s.cwd.is_empty() { "*" } else { &s.cwd },
            s.policy
                .map(|p| format!("{p:?}"))
                .unwrap_or_else(|| "→ run.default_policy".into())
        );
    }
    0
}

/// `kp _report`：pane-died hook / _spawn 回传退出码；daemon 不可达时兜底直写历史。
pub fn cmd_report(name: &str, code: &str) -> i32 {
    let code = if code.is_empty() {
        None
    } else {
        code.parse::<i32>().ok()
    };
    match rpc(Request::ReportDone {
        name: name.to_string(),
        exit_code: code,
    }) {
        Ok(Response::Ok) => 0,
        _ => {
            if let (Ok(specs), Ok(hf)) = (paths::specs_dir(), paths::history_file()) {
                let sp = specs.join(format!("{name}.json"));
                if let Ok(text) = std::fs::read_to_string(&sp) {
                    if let Ok(spec) = serde_json::from_str::<SessionSpec>(&text) {
                        let _ = history::append(
                            &hf,
                            &HistoryEntry {
                                name: name.to_string(),
                                argv: spec.argv,
                                cwd: spec.cwd.to_string_lossy().to_string(),
                                branch: spec.branch,
                                policy: spec.policy,
                                started_at_ms: 0,
                                ended_at_ms: now_ms(),
                                exit_code: code,
                                reason: "exited".into(),
                            },
                        );
                    }
                }
            }
            0
        }
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn rejected_outcome_returns_3() {
        let info = crate::protocol::SessionInfo {
            name: "kp-x".into(),
            ..Default::default()
        };
        let code = handle_outcome(&SpawnOutcome::Rejected { info });
        assert_eq!(code, 3, "reject 策略退出码应为 3（FR3）");
    }

    #[test]
    fn internal_cmd_guard() {
        assert!(kp_cli_internal("_spawn"));
        assert!(kp_cli_internal("_holder"));
        assert!(kp_cli_internal("_attach"));
        assert!(kp_cli_internal("_report"));
        assert!(!kp_cli_internal("ls"));
        assert!(!kp_cli_internal("sleep"));
    }
}
