//! kp — keep processes running：透明命令包装 + 会话去重 + tmux 引擎。
//! 架构见 docs/05-architecture.md。

mod client;
mod config;
mod daemon;
mod engine;
mod glob;
mod history;
mod identity;
mod paths;
mod protocol;
mod registry;
mod spawn_cmd;
mod util;

use clap::{Parser, Subcommand};
use protocol::Policy;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "kp",
    version,
    about = "kp — 让命令保持运行：透明包装 + 会话去重 + tmux 引擎",
    allow_external_subcommands = true
)]
struct Cli {
    /// 会话名（显式身份；最终为 kp-<项目>-<NAME>）
    #[arg(short, long)]
    name: Option<String>,

    /// 重复命令时采用的策略（attach|wait|restart|new|reject）
    #[arg(short, long, value_enum)]
    policy: Option<Policy>,

    /// 运行后台 daemon（内部使用）
    #[arg(long, hide = true)]
    daemon: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 列出所有会话
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
    /// 重启会话
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
    #[command(hide = true, name = "_spawn")]
    Spawn {
        #[arg(long)]
        spec: PathBuf,
    },
    #[command(hide = true, name = "_report")]
    Report { name: String, code: String },
    /// 被包装的命令（kp CMD...）
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

fn main() {
    let cli = Cli::parse();
    let code = if cli.daemon {
        match daemon::serve() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("kp daemon: {e}");
                1
            }
        }
    } else {
        match cli.cmd {
            Some(Cmd::External(argv)) => {
                let args: Vec<String> = argv
                    .into_iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                client::cmd_run(cli.name, cli.policy, args)
            }
            Some(Cmd::Ls) => client::cmd_ls(),
            Some(Cmd::Status { name }) => client::cmd_status(&name),
            Some(Cmd::Attach { name }) => client::cmd_attach(&name),
            Some(Cmd::Stop { name, all }) => client::cmd_stop(name, all),
            Some(Cmd::Restart { name, fresh_env }) => client::cmd_restart(&name, fresh_env),
            Some(Cmd::Logs {
                name,
                follow,
                lines,
            }) => client::cmd_logs(&name, follow, lines),
            Some(Cmd::History { limit }) => client::cmd_history(limit),
            Some(Cmd::Clean) => client::cmd_clean(),
            Some(Cmd::Config) => client::cmd_config(),
            Some(Cmd::Spawn { spec }) => spawn_cmd::run(&spec),
            Some(Cmd::Report { name, code }) => client::cmd_report(&name, &code),
            None => {
                use clap::CommandFactory;
                let _ = Cli::command().print_help();
                2
            }
        }
    };
    std::process::exit(code);
}
