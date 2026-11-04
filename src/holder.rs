//! `kp _holder`：native 引擎的每会话持有进程（架构 D12 / dtach 式）。
//!
//! setsid → openpty → fork（子进程 setsid+TIOCSCTTY+exec 真实命令）→ 监听会话 socket：
//! - PTY 输出广播给所有 attach 客户端，并 tee 落盘（自带 log_max_mb 限额）；
//! - 多读单写在 holder 侧强制：写锁先到先得，写锁释放后可被后续客户端认领；
//! - SIGCHLD → waitpid 拿真实退出码（信号死亡 130 可达）→ 写状态文件 + 上报 daemon；
//! - holder 是 detached 的独立进程：daemon 崩溃 / tmux 不存在都不影响会话（D6）。

use crate::engine::native::{session_sock_path, session_status_path, HolderStatus};
use crate::paths;
use crate::protocol::{recv_frame, send_frame, Request, Response, SessionSpec, PROTO_VERSION};
use crate::util::now_ms;
use serde_json::json;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

const GRACE_MS: u64 = 2000; // 子进程退出后给 attach 端的收尾窗口
const LOG_CHECK_EVERY: u64 = 4096 * 16; // 每累计写入量做一次限额检查

// ---- 帧协议（holder socket）：4 字节 BE 长度 + 1 字节类型 + 载荷 ----
pub const F_ATTACH: u8 = b'A'; // C→H JSON {readonly, cols, rows}
pub const F_INPUT: u8 = b'I'; // C→H 原始字节（仅写客户端）
pub const F_RESIZE: u8 = b'R'; // C→H JSON {cols, rows}
pub const F_KILL: u8 = b'K'; // C→H 无载荷（daemon kill_session 通道）
pub const F_WELCOME: u8 = b'W'; // H→C JSON {role, clients}
pub const F_DATA: u8 = b'D'; // H→C 原始字节（PTY 输出）
pub const F_EXITED: u8 = b'X'; // H→C JSON {code}

pub fn frame(t: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 5);
    v.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
    v.push(t);
    v.extend_from_slice(payload);
    v
}

static WAKE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_chld(_: libc::c_int) {
    let fd = WAKE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = b"x";
        unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1) };
    }
}

struct Client {
    stream: UnixStream,
    writer: bool,
    out: Vec<u8>,
    inbuf: Vec<u8>,
}

pub fn run(spec_path: &Path) -> i32 {
    let Ok(text) = std::fs::read_to_string(spec_path) else {
        eprintln!("kp _holder: 读取 spec 失败 {}", spec_path.display());
        return 127;
    };
    let Ok(spec) = serde_json::from_str::<SessionSpec>(&text) else {
        eprintln!("kp _holder: 解析 spec 失败");
        return 127;
    };

    unsafe {
        // holder 自成会话：pid = pgid = sid；daemon 整组杀灭针对子进程组，不波及 holder，
        // 因此 holder 正常响应 TERM/INT/QUIT（运维可清理），仅 SIGPIPE 保持默认
        libc::setsid();
    }
    // SIGCHLD 自管道（异步信号安全的唤醒方式）
    let mut pipefd = [0 as RawFd; 2];
    unsafe {
        if libc::pipe(pipefd.as_mut_ptr()) != 0 {
            eprintln!("kp _holder: pipe 失败");
            return 1;
        }
        for fd in pipefd {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        libc::signal(libc::SIGCHLD, on_chld as *const () as usize);
    }
    WAKE_W.store(pipefd[1], Ordering::Relaxed);

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    unsafe {
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
        {
            eprintln!("kp _holder: openpty 失败");
            return 1;
        }
    }

    let child_pid = unsafe { libc::fork() };
    if child_pid < 0 {
        eprintln!("kp _holder: fork 失败");
        return 1;
    }
    if child_pid == 0 {
        child_exec(&spec, master, slave); // 不返回
    }
    unsafe { libc::close(slave) };

    let (Ok(status_path), Ok(sock_path)) = (
        session_status_path(&spec.name),
        session_sock_path(&spec.name),
    ) else {
        eprintln!("kp _holder: 运行时目录不可用");
        return 1;
    };
    if let Some(dir) = status_path.parent() {
        let _ = paths::ensure_dir(dir);
    }
    let _ = std::fs::remove_file(&sock_path);
    let Ok(listener) = UnixListener::bind(&sock_path) else {
        eprintln!("kp _holder: 绑定会话 socket 失败 {}", sock_path.display());
        return 1;
    };
    let _ = listener.set_nonblocking(true);
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL, 0);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let mut status = HolderStatus {
        name: spec.name.clone(),
        pid: child_pid,
        pgid: child_pid, // 子进程 setsid 后自己的 pgid = pid
        holder_pid: std::process::id() as i32,
        started_at_ms: now_ms(),
        status: "running".into(),
        clients: 0,
        exit_code: None,
    };
    write_status(&status_path, &status);

    // 日志：追加打开，自带限额
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_path)
        .ok();
    let mut log_bytes = std::fs::metadata(&spec.log_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let log_max = spec.log_max_mb.saturating_mul(1024 * 1024);
    let mut log_unwritten = 0u64;

    let mut clients: Vec<Client> = Vec::new();
    let mut writer_seen = false;
    let mut pty_open = true;
    let mut pty_out: Vec<u8> = Vec::new(); // master 写入积压
    let mut exited_code: Option<i32> = None;

    let master_fd = master;
    let listener_fd = listener.as_raw_fd();
    let wake_fd = pipefd[0];
    'main: loop {
        let mut fds: Vec<libc::pollfd> = Vec::new();
        let mut idx_master = None;
        if pty_open {
            idx_master = Some(fds.len());
            fds.push(libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN | if pty_out.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            });
        }
        let idx_listen = fds.len();
        fds.push(libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        let idx_wake = fds.len();
        fds.push(libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        for c in &clients {
            fds.push(libc::pollfd {
                fd: c.stream.as_raw_fd(),
                events: libc::POLLIN | if c.out.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            });
        }
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 500) };
        if n < 0 && errno() != libc::EINTR {
            break;
        }

        // SIGCHLD → 收尸
        if fds[idx_wake].revents & libc::POLLIN != 0 {
            let mut b = [0u8; 64];
            unsafe { libc::read(wake_fd, b.as_mut_ptr() as *mut libc::c_void, 64) };
            let mut st = 0 as libc::c_int;
            loop {
                let r = unsafe { libc::waitpid(child_pid, &mut st, libc::WNOHANG) };
                if r <= 0 {
                    break;
                }
                let code = if libc::WIFEXITED(st) {
                    libc::WEXITSTATUS(st)
                } else if libc::WIFSIGNALED(st) {
                    128 + libc::WTERMSIG(st)
                } else {
                    continue;
                };
                exited_code = Some(code);
            }
            if let Some(code) = exited_code {
                status.status = "exited".into();
                status.exit_code = Some(code);
                let payload = serde_json::to_vec(&json!({ "code": code })).unwrap_or_default();
                let f = frame(F_EXITED, &payload);
                for c in clients.iter_mut() {
                    c.out.extend_from_slice(&f);
                }
                report_daemon(&spec.name, code);
                break 'main;
            }
        }

        // master 可读 → 广播 + 落盘
        if let Some(i) = idx_master {
            if fds[i].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let mut buf = [0u8; 65536];
                let r = unsafe {
                    libc::read(master_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if r > 0 {
                    let chunk = &buf[..r as usize];
                    let f = frame(F_DATA, chunk);
                    for c in clients.iter_mut() {
                        c.out.extend_from_slice(&f);
                    }
                    if let Some(l) = log.as_mut() {
                        let _ = l.write_all(chunk);
                        log_bytes += r as u64;
                        log_unwritten += r as u64;
                        if log_max > 0 && log_unwritten >= LOG_CHECK_EVERY && log_bytes > log_max {
                            // 超限截断（与 tmux 路径 janitor 语义一致）
                            log = std::fs::OpenOptions::new()
                                .write(true)
                                .truncate(true)
                                .create(true)
                                .open(&spec.log_path)
                                .ok();
                            log_bytes = 0;
                            log_unwritten = 0;
                        }
                    }
                } else if r == 0 || errno() == libc::EIO {
                    pty_open = false;
                }
            }
            if pty_open && !pty_out.is_empty() && fds[i].revents & libc::POLLOUT != 0 {
                flush_master(master_fd, &mut pty_out);
            }
        }

        // 新客户端（Attach 或 daemon 的 Kill）
        if fds[idx_listen].revents & libc::POLLIN != 0 {
            if let Ok((mut stream, _)) = listener.accept() {
                eprintln!("kp _holder[{}]: 新连接 @{}", spec.name, now_ms());
                // macOS/BSD 的 accept 继承 listener 的 O_NONBLOCK：读首帧必须先恢复阻塞
                let _ = stream.set_nonblocking(false);
                match read_first_frame(&mut stream) {
                    Err(e) => eprintln!("kp _holder[{}]: 首帧解析失败: {e}", spec.name),
                    Ok(FirstFrame::Attach(att)) => {
                        let _ = stream.set_nonblocking(true);
                        let want_writer = !att.readonly && !writer_seen;
                        if want_writer {
                            writer_seen = true;
                        }
                        let role = if want_writer { "write" } else { "read" };
                        status.clients += 1;
                        write_status(&status_path, &status);
                        if att.cols > 0 && att.rows > 0 && (want_writer || clients.is_empty()) {
                            set_winsize(master_fd, att.cols, att.rows, child_pid);
                        }
                        let welcome = frame(
                            F_WELCOME,
                            &serde_json::to_vec(&json!({
                                "role": role,
                                "clients": status.clients
                            }))
                            .unwrap_or_default(),
                        );
                        eprintln!(
                            "kp _holder[{}]: attach 授权 role={role} clients={} @{}",
                            spec.name,
                            status.clients,
                            now_ms()
                        );
                        clients.push(Client {
                            stream,
                            writer: want_writer,
                            out: welcome,
                            inbuf: Vec::new(),
                        });
                        // 新客户端本轮无 poll 槽位：跳过本轮客户端 I/O（下一轮纳入）
                        continue 'main;
                    }
                    Ok(FirstFrame::Kill) => {
                        // kill_session 通道：TERM 子进程组，SIGCHLD 走正常收尸路径
                        eprintln!("kp _holder[{}]: 收到 Kill 帧", spec.name);
                        unsafe { libc::kill(-child_pid, libc::SIGTERM) };
                    }
                }
            }
        }

        // 客户端 I/O（数组顺序：master? → listener → wake → clients…，
        // wake 在 listener 之后，故 clients 从 idx_listen + 2 开始）
        let base = idx_listen + 2;
        let mut dead: Vec<usize> = Vec::new();
        for (ci, c) in clients.iter_mut().enumerate() {
            let ev = fds[base + ci].revents;
            if ev & libc::POLLOUT != 0 && !c.out.is_empty() {
                match c.stream.write(&c.out) {
                    Ok(n) if n > 0 => {
                        c.out.drain(..n);
                    }
                    // Ok(0)：对端已死但缓冲未满——不处理会 POLLOUT 永久自旋
                    Ok(_) => dead.push(ci),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => dead.push(ci),
                }
            }
            if ev & (libc::POLLIN | libc::POLLHUP) != 0 {
                let mut buf = [0u8; 16384];
                match c.stream.read(&mut buf) {
                    Ok(0) => dead.push(ci),
                    Ok(n) => {
                        c.inbuf.extend_from_slice(&buf[..n]);
                        loop {
                            if c.inbuf.len() < 4 {
                                break;
                            }
                            let len = u32::from_be_bytes([
                                c.inbuf[0], c.inbuf[1], c.inbuf[2], c.inbuf[3],
                            ]) as usize;
                            if len > 8 * 1024 * 1024 || c.inbuf.len() < 4 + len {
                                break;
                            }
                            c.inbuf.drain(..4); // 剥长度头
                            let payload: Vec<u8> = c.inbuf.drain(..len).collect();
                            match payload[0] {
                                F_INPUT if c.writer && pty_open && exited_code.is_none() => {
                                    if std::env::var_os("KP_HOLDER_TRACE").is_some() {
                                        eprintln!(
                                            "kp _holder[{}]: F_INPUT {} 字节",
                                            spec.name,
                                            payload.len() - 1
                                        );
                                    }
                                    pty_out.extend_from_slice(&payload[1..]);
                                    flush_master(master_fd, &mut pty_out);
                                    if std::env::var_os("KP_HOLDER_TRACE").is_some() {
                                        eprintln!(
                                            "kp _holder[{}]: master 写后积压 {}",
                                            spec.name,
                                            pty_out.len()
                                        );
                                    }
                                }
                                F_RESIZE => {
                                    if let Ok(v) =
                                        serde_json::from_slice::<serde_json::Value>(&payload[1..])
                                    {
                                        let cols = v["cols"].as_u64().unwrap_or(0) as u16;
                                        let rows = v["rows"].as_u64().unwrap_or(0) as u16;
                                        if cols > 0 && rows > 0 {
                                            set_winsize(master_fd, cols, rows, child_pid);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => dead.push(ci),
                }
            }
        }
        for ci in dead.into_iter().rev() {
            if clients[ci].writer {
                writer_seen = false; // 写锁可被后续客户端认领
            }
            clients.remove(ci);
            status.clients = clients.len() as u32;
            write_status(&status_path, &status);
        }
    }

    // grace：继续中继残余输出，给 attach 端收尾窗口
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(GRACE_MS);
    while std::time::Instant::now() < deadline {
        if pty_open {
            let mut buf = [0u8; 65536];
            let r =
                unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if r > 0 {
                let f = frame(F_DATA, &buf[..r as usize]);
                for c in clients.iter_mut() {
                    c.out.extend_from_slice(&f);
                }
                if let Some(l) = log.as_mut() {
                    let _ = l.write_all(&buf[..r as usize]);
                }
            } else if r == 0 || errno() == libc::EIO {
                pty_open = false;
            }
        }
        for c in clients.iter_mut() {
            if !c.out.is_empty() {
                let _ = c.stream.write_all(&c.out);
                c.out.clear();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(&sock_path);
    write_status(&status_path, &status);
    0
}

fn flush_master(master: RawFd, buf: &mut Vec<u8>) {
    while !buf.is_empty() {
        let w = unsafe { libc::write(master, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if w > 0 {
            buf.drain(..w as usize);
        } else {
            break; // EAGAIN：留待 POLLOUT
        }
    }
}

enum FirstFrame {
    Attach(AttachReq),
    Kill,
}

struct AttachReq {
    readonly: bool,
    cols: u16,
    rows: u16,
}

fn read_first_frame(stream: &mut UnixStream) -> Result<FirstFrame, String> {
    let mut lenb = [0u8; 4];
    stream.read_exact(&mut lenb).map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(lenb) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).map_err(|e| e.to_string())?;
    match buf.first() {
        Some(&F_ATTACH) => {
            let v: serde_json::Value =
                serde_json::from_slice(&buf[1..]).map_err(|e| e.to_string())?;
            Ok(FirstFrame::Attach(AttachReq {
                readonly: v["readonly"].as_bool().unwrap_or(true),
                cols: v["cols"].as_u64().unwrap_or(0) as u16,
                rows: v["rows"].as_u64().unwrap_or(0) as u16,
            }))
        }
        Some(&F_KILL) => Ok(FirstFrame::Kill),
        _ => Err("未知首帧".into()),
    }
}

fn set_winsize(master: RawFd, cols: u16, rows: u16, child_pid: i32) {
    unsafe {
        let ws = libc::winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
        if child_pid > 0 {
            libc::kill(-child_pid, libc::SIGWINCH);
        }
    }
}

fn write_status(path: &Path, s: &HolderStatus) {
    let tmp = path.with_extension("json.tmp");
    if let Ok(j) = serde_json::to_string(s) {
        if std::fs::write(&tmp, j).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// 退出码即时上报 daemon（janitor 轮询是兜底路径）。
fn report_daemon(name: &str, code: i32) {
    let Ok(sock) = paths::socket_path() else {
        return;
    };
    let Ok(mut s) = UnixStream::connect(sock) else {
        return;
    };
    let _ = send_frame(
        &mut s,
        &Request::Hello {
            version: PROTO_VERSION,
        },
    );
    if matches!(recv_frame::<Response>(&mut s), Ok(Response::Hello { .. })) {
        let _ = send_frame(
            &mut s,
            &Request::ReportDone {
                name: name.to_string(),
                exit_code: Some(code),
            },
        );
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// 子进程分支：新会话 + 控制终端 + 默认信号处置 + env 快照 + exec。
fn child_exec(spec: &SessionSpec, master: RawFd, slave: RawFd) -> ! {
    unsafe {
        libc::setsid();
        libc::ioctl(slave, TIOCSCTTY, 0 as libc::c_ulong);
        libc::dup2(slave, 0);
        libc::dup2(slave, 1);
        libc::dup2(slave, 2);
        if slave > 2 {
            libc::close(slave);
        }
        libc::close(master);
        // 撤销 holder 的 IGN 恢复默认处置；Rust 运行时的 SIGPIPE=IGN 也复位
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGQUIT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if std::env::set_current_dir(&spec.cwd).is_err() {
        eprintln!("kp _holder: 无法进入工作目录 {}", spec.cwd.display());
        std::process::exit(1);
    }
    let keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    for k in keys {
        std::env::remove_var(&k);
    }
    for (k, v) in &spec.env {
        std::env::set_var(k, v);
    }
    std::env::set_var("KP_SESSION_ID", &spec.name);
    if spec.argv.is_empty() {
        std::process::exit(127);
    }
    // exec 只会失败返回；std 创建的 fd 均带 CLOEXEC，无需手工清理
    let err = std::process::Command::new(&spec.argv[0])
        .args(&spec.argv[1..])
        .exec();
    eprintln!("kp _holder: exec {} 失败: {err}", spec.argv[0]);
    std::process::exit(127);
}

#[cfg(target_os = "macos")]
const TIOCSCTTY: libc::c_ulong = 0x20007461;
#[cfg(not(target_os = "macos"))]
const TIOCSCTTY: libc::c_ulong = libc::TIOCSCTTY as libc::c_ulong;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout() {
        let f = frame(F_ATTACH, b"{\"a\":1}");
        assert_eq!(&f[..4], &(7u32 + 1).to_be_bytes()); // 长度 = 载荷 + 类型字节
        assert_eq!(f[4], F_ATTACH);
        assert_eq!(&f[5..], b"{\"a\":1}");
        assert_eq!(f.len(), 4 + 1 + 7);
    }

    #[test]
    fn frame_empty_payload() {
        let f = frame(F_KILL, &[]);
        assert_eq!(f, vec![0, 0, 0, 1, F_KILL]);
    }
}
