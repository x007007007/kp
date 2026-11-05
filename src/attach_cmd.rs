//! `kp _attach`：native 引擎的客户端 attach 辅助进程。
//! raw 模式中继：stdin → holder（写客户端），PTY 输出 → stdout；
//! SIGWINCH → Resize；收到 Exited 帧后还原终端并以子进程退出码退出。
//! 前缀键（tmux 同款）：Ctrl-b d = 脱离（会话继续跑）；Ctrl-b Ctrl-b = 发送字面 Ctrl-b；
//! Ctrl-b + 其他键 = 前缀与该键原样透传（不吞键）。

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
    let mut prefix = false; // Ctrl-b 前缀模式（跨 read 保持）
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
                let (bytes, detach) = prefix_filter(&mut prefix, &buf[..r as usize]);
                if !bytes.is_empty() {
                    let _ = stream.write_all(&frame(F_INPUT, &bytes));
                }
                if detach {
                    break 'main; // Ctrl-b d：脱离（会话继续跑；终端还原在循环后统一做）
                }
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

/// 前缀键状态机（tmux 同款语义，native 引擎 detach 的唯一入口）：
/// - 普通模式收到 0x02（Ctrl-b）→ 进入前缀模式，本字节暂不转发；
/// - 前缀模式收到 `d` → 脱离（返回 detach=true，字节不转发）；
/// - 前缀模式收到 0x02 → 转发一个字面量 Ctrl-b；
/// - 前缀模式收到其他字节 → 前缀 + 该字节原样转发（透明不吞键，如 emacs 的 C-b）。
fn prefix_filter(prefix: &mut bool, buf: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(buf.len() + 1);
    for &b in buf {
        if *prefix {
            *prefix = false;
            if b == b'd' {
                return (out, true);
            }
            out.push(0x02);
            if b != 0x02 {
                out.push(b);
            }
        } else if b == 0x02 {
            *prefix = true;
        } else {
            out.push(b);
        }
    }
    (out, false)
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

#[cfg(test)]
mod tests {
    use super::prefix_filter;

    /// 返回 (转发字节, 是否 detach, 前缀模式是否仍挂起)
    fn f(prefix: bool, buf: &[u8]) -> (Vec<u8>, bool, bool) {
        let mut p = prefix;
        let (out, detach) = prefix_filter(&mut p, buf);
        (out, detach, p)
    }

    #[test]
    fn plain_bytes_pass_through() {
        let (out, detach, p) = f(false, b"abc\n");
        assert_eq!(out, b"abc\n");
        assert!(!detach && !p);
    }

    #[test]
    fn prefix_d_detaches_without_forwarding() {
        let (out, detach, p) = f(false, &[0x02, b'd']);
        assert!(out.is_empty() && detach && !p);
    }

    #[test]
    fn prefix_prefix_sends_literal_ctrl_b() {
        let (out, detach, p) = f(false, &[0x02, 0x02]);
        assert_eq!(out, &[0x02]);
        assert!(!detach && !p);
    }

    #[test]
    fn prefix_other_forwards_both_bytes() {
        let (out, detach, p) = f(false, &[0x02, b'x']);
        assert_eq!(out, &[0x02, b'x']);
        assert!(!detach && !p);
    }

    #[test]
    fn prefix_survives_read_split() {
        // 前缀与其后按键分属两次 read：模式跨 read 保持
        let (out1, detach, p) = f(false, &[0x02]);
        assert!(out1.is_empty() && !detach && p);
        let (out2, detach, p) = f(p, &[b'd']);
        assert!(out2.is_empty() && detach && !p);
    }

    #[test]
    fn literal_prefix_then_plain_d_is_not_detach() {
        // C-b C-b d → 字面 C-b + 普通 d（前缀已被第二个 C-b 消费），不脱离
        let (out, detach, p) = f(false, &[0x02, 0x02, b'd']);
        assert_eq!(out, &[0x02, b'd']);
        assert!(!detach && !p);
    }

    #[test]
    fn lone_prefix_at_buffer_end_stays_pending() {
        let (out, detach, p) = f(false, &[0x02]);
        assert!(out.is_empty() && !detach && p);
    }
}
