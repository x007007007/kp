//! `kp _spawn`：tmux pane 的入口进程。
//! 载入 spec → 全量替换 environ（快照语义，FR2）→ fork 真实命令并 waitpid。
//!
//! 不用 exec 顶替的原因：tmux 的 `#{pane_dead_status}` 对信号死亡返回空，
//! Ctrl-C（FR5 最常见的退出路径）的退出码只有 waitpid 才拿得到；
//! fork 后 _spawn 仍是进程组 leader（pane pid 不变），整组杀灭语义不受影响。
//! 上报走 RPC（`_report`），pane-died hook 退为兜底路径。

use crate::protocol::SessionSpec;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::Command;

pub fn run(spec_path: &Path) -> i32 {
    let text = match std::fs::read_to_string(spec_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("kp _spawn: 读取 spec 失败 {}: {e}", spec_path.display());
            return 127;
        }
    };
    let spec: SessionSpec = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kp _spawn: 解析 spec 失败: {e}");
            return 127;
        }
    };

    let keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    for k in keys {
        std::env::remove_var(k);
    }
    for (k, v) in &spec.env {
        std::env::set_var(k, v);
    }
    std::env::set_var("KP_SESSION_ID", &spec.name);

    if let Err(e) = std::env::set_current_dir(&spec.cwd) {
        eprintln!("kp _spawn: 无法进入工作目录 {}: {e}", spec.cwd.display());
        return 1;
    }
    if spec.argv.is_empty() {
        eprintln!("kp _spawn: spec.argv 为空");
        return 127;
    }

    // 父进程屏蔽终止类信号：Ctrl-C/整组杀灭时活下来收尸并上报退出码
    // （tmux 的 pane_dead_status 对信号死亡为空，退出码只能由 waitpid 获得）。
    // 子进程在 pre_exec 里恢复默认处置，Ctrl-C 语义不受影响（FR5）。
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    unsafe {
        cmd.pre_exec(|| {
            // 恢复默认处置 + 撤销 Rust 运行时的 SIGPIPE=IGN，让真实命令行为如常
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kp _spawn: 启动 {} 失败: {e}", spec.argv[0]);
            return 127;
        }
    };
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kp _spawn: waitpid 失败: {e}");
            return 1;
        }
    };
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    // 主动上报真实退出码（含信号死亡）；失败时 hook/janitor 兜底
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .args(["_report", &spec.name, &code.to_string()])
            .status();
    }
    code
}
