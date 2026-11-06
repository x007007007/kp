//! kpm — kp 会话管理：查看、接入、停止、重启、日志、历史、配置。
//! kp 本体只负责包装命令（kp CMD...），管理入口全部集中在这里（D10）。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "kpm",
    version,
    about = "kpm — kp 会话管理（kp 本体只负责包装命令：kp CMD...）"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 列出所有会话（含启动策略）
    Ls,
    /// 查看单个会话详情
    Status { name: String },
    /// 接入既有会话
    Attach { name: String },
    /// 停止会话（杀整组：TERM → 超时 → KILL）
    Stop {
        name: Option<String>,
        /// 停止所有会话
        #[arg(long)]
        all: bool,
    },
    /// 重启会话（已 attach 的窗口自动跟随接入新实例）
    Restart {
        name: String,
        /// 用当前 shell 环境替代启动时的环境快照
        #[arg(long)]
        fresh_env: bool,
    },
    /// 查看会话日志
    Logs {
        name: String,
        /// 跟随输出（tail -f）
        #[arg(short, long)]
        follow: bool,
        /// 尾部行数
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// 已结束会话的退出码与时长
    History {
        /// 显示条数
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// 清理已退出会话、spec 与过期历史
    Clean,
    /// 打印合并后的配置与 services 候选链
    Config,
    /// 排障信息：二进制版本、daemon 身份、会话概览、日志尾部
    Doctor,
}

fn main() {
    // 管道下游关闭（如 kpm status | grep -q）时静默退出，而非 println panic
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let cli = Cli::parse();
    use kp_cli::client;
    let code = match cli.cmd {
        Cmd::Ls => client::cmd_ls(),
        Cmd::Status { name } => client::cmd_status(&name),
        Cmd::Attach { name } => client::cmd_attach(&name),
        Cmd::Stop { name, all } => client::cmd_stop(name, all),
        Cmd::Restart { name, fresh_env } => client::cmd_restart(&name, fresh_env),
        Cmd::Logs {
            name,
            follow,
            lines,
        } => client::cmd_logs(&name, follow, lines),
        Cmd::History { limit } => client::cmd_history(limit),
        Cmd::Clean => client::cmd_clean(),
        Cmd::Config => client::cmd_config(),
        Cmd::Doctor => client::cmd_doctor(),
    };
    std::process::exit(code);
}
