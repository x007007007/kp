//! `kp _attach`：native 引擎的客户端 attach 辅助进程。
//! raw 模式中继：stdin → holder（写客户端），PTY 输出 → stdout；
//! SIGWINCH → Resize；收到 Exited 帧后还原终端并以子进程退出码退出。

use crate::holder::{frame, F_ATTACH, F_DATA, F_EXITED, F_INPUT, F_RESIZE, F_WELCOME};
use serde_json::json;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

pub fn run(sock: &std::path::Path, readonly: bool) -> i32 {
    let Ok(mut stream) = UnixStream::connect(sock) else {
        eprintln!("kp: 无法连接会话（holder 未运行？）");
        return 1;
    };
    // stream 保持阻塞模式：读侧由 poll 控速，写侧数据量小由 holder 快速消费
    let stdin_fd = 0 as RawFd;
    let (mut cols, mut rows) = winsize(stdin_fd);

    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    // SIGWINCH 自管道
    let mut pipefd = [0 as RawFd; 2];
    static WAKE_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    unsafe {
        if libc::pipe(pipefd.as_mut_ptr()) != 0 {
            return 1;
        }
        for fd in pipefd {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        WAKE_W.store(pipefd[1], std::sync::atomic::Ordering::Relaxed);
        extern "C" fn on_winch(_: libc::c_int) {
            let fd = WAKE_W.load(std::sync::atomic::Ordering::Relaxed);
            if fd >= 0 {
                let b = b"x";
                unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1) };
            }
        }
        libc::signal(libc::SIGWINCH, on_winch as *const () as usize);
    }

    // raw 模式（Ctrl-C 变成 0x03 字节流向 PTY，等价于发给会话进程）
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let raw_ok = unsafe {
        if libc::tcgetattr(stdin_fd, &mut saved) == 0 {
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw) == 0
        } else {
            false
        }
    };

    let hello = frame(
        F_ATTACH,
        &serde_json::to_vec(&json!({ "readonly": readonly, "cols": cols, "rows": rows }))
            .unwrap_or_default(),
    );
    if stream.write_all(&hello).is_err() {
        if raw_ok {
            unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &saved) };
        }
        eprintln!("kp: 会话握手失败");
        return 1;
    }

    let mut inbuf: Vec<u8> = Vec::new();
    let mut out_buf: Vec<u8> = Vec::new();
    let mut stdin_open = true;
    let mut exit_code: i32 = 0;
    let sock_fd = stream.as_raw_fd();
    let wake_fd = pipefd[0];

    'main: loop {
        let mut fds = [
            libc::pollfd {
                fd: stdin_fd,
                events: if stdin_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: sock_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 3, 500) };
        if n < 0 {
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if e != libc::EINTR {
                break;
            }
        }
        // 终端尺寸变化
        if fds[2].revents & libc::POLLIN != 0 {
            let mut b = [0u8; 32];
            unsafe { libc::read(wake_fd, b.as_mut_ptr() as *mut libc::c_void, 32) };
            let (c2, r2) = winsize(stdin_fd);
            if (c2, r2) != (cols, rows) {
                cols = c2;
                rows = r2;
                let f = frame(
                    F_RESIZE,
                    &serde_json::to_vec(&json!({ "cols": cols, "rows": rows })).unwrap_or_default(),
                );
                let _ = stream.write_all(&f);
            }
        }
        // 键盘 → holder
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 4096];
            let r =
                unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if r == 0 {
                stdin_open = false; // stdin 关闭（如 </dev/null）：停止轮询，不空转
            } else if r > 0 && !readonly {
                let _ = stream.write_all(&frame(F_INPUT, &buf[..r as usize]));
            }
        }
        // holder → 终端
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut buf = [0u8; 65536];
            match stream.read(&mut buf) {
                Ok(0) => {
                    if std::env::var_os("KP_ATTACH_TRACE").is_some() {
                        eprintln!("_attach: holder EOF");
                    }
                    break; // holder 关闭
                }
                Ok(n) => {
                    inbuf.extend_from_slice(&buf[..n]);
                    loop {
                        if inbuf.len() < 4 {
                            break;
                        }
                        let len =
                            u32::from_be_bytes([inbuf[0], inbuf[1], inbuf[2], inbuf[3]]) as usize;
                        if len > 8 * 1024 * 1024 || inbuf.len() < 4 + len {
                            break;
                        }
                        inbuf.drain(..4); // 剥长度头
                        let payload: Vec<u8> = inbuf.drain(..len).collect();
                        if std::env::var_os("KP_ATTACH_TRACE").is_some() {
                            eprintln!(
                                "_attach: 帧 type={:?} len={} hex={:?}",
                                payload[0] as char,
                                len,
                                payload
                                    .iter()
                                    .take(12)
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<String>()
                            );
                        }
                        match payload[0] {
                            F_DATA => out_buf.extend_from_slice(&payload[1..]),
                            F_WELCOME => {
                                if let Ok(v) =
                                    serde_json::from_slice::<serde_json::Value>(&payload[1..])
                                {
                                    if v["role"] == "read" && !readonly {
                                        // 写锁被占，holder 降级为只读
                                        let notice = "\r\nkp: 写锁被占用，本次接入为只读模式\r\n";
                                        let _ = write(1, notice.as_bytes());
                                    }
                                }
                            }
                            F_EXITED => {
                                if let Ok(v) =
                                    serde_json::from_slice::<serde_json::Value>(&payload[1..])
                                {
                                    exit_code = v["code"].as_i64().unwrap_or(0) as i32;
                                }
                                break 'main;
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }
        if !out_buf.is_empty() {
            let w = write(1, &out_buf);
            if w > 0 {
                out_buf.drain(..w);
            }
        }
    }
    if raw_ok {
        unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &saved) };
    }
    exit_code
}

fn winsize(fd: RawFd) -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

fn write(fd: RawFd, data: &[u8]) -> usize {
    unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()).max(0) as usize }
}
