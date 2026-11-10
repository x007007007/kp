//! kp — 包装任意命令。没有子命令：kp 之后的一切都是被包装的命令及其参数，
//! 因此 `kp ls`、`kp help`、`kp restart` 都能包装同名系统命令（D10）。
//! 会话管理命令在 `kpm`；`_spawn`/`_holder`/`_attach`/`_report` 为内部命令。

use clap::Parser;
use kp_cli::i18n;
use kp_cli::protocol::Policy;
use kp_cli::{attach_cmd, client, daemon, holder, spawn_cmd};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "kp",
    version,
    about = "kp — keep processes running：把任意命令交给后台，重复命令自动去重/接入（管理命令见 kpm）",
    trailing_var_arg = true
)]
struct Cli {
    /// 会话名（显式身份；最终为 kp-<项目>-<NAME>）
    #[arg(short, long)]
    name: Option<String>,

    /// 重复时采用的策略（attach|wait|restart|new|reject）；设置一次后对该服务粘性生效
    #[arg(short, long, value_enum)]
    policy: Option<Policy>,

    /// 运行后台 daemon（内部使用）
    #[arg(long, hide = true)]
    daemon: bool,

    /// 被包装的命令及其参数
    #[arg(hide = true)]
    args: Vec<OsString>,
}

fn main() {
    // 管道下游关闭（如 kpm status | grep -q）时静默退出，而非 println panic
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let cli = Cli::parse();
    if cli.daemon {
        let code = match daemon::serve() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("kp daemon: {e}");
                1
            }
        };
        std::process::exit(code);
    }
    let args: Vec<String> = cli
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let code = match args.split_first() {
        Some((a, rest)) if a == "_spawn" => match parse_flag(rest, "--spec") {
            Some(p) => spawn_cmd::run(&PathBuf::from(p)),
            None => {
                eprintln!("kp _spawn: {}", i18n::s("缺少 --spec", "missing --spec"));
                2
            }
        },
        Some((a, rest)) if a == "_holder" => match parse_flag(rest, "--spec") {
            Some(p) => holder::run(&PathBuf::from(p)),
            None => {
                eprintln!("kp _holder: {}", i18n::s("缺少 --spec", "missing --spec"));
                2
            }
        },
        Some((a, rest)) if a == "_attach" => {
            let sock = parse_flag(rest, "--socket").map(PathBuf::from);
            let readonly = rest.iter().any(|a| a == "--readonly");
            match sock {
                Some(s) => attach_cmd::run(&s, readonly),
                None => {
                    eprintln!(
                        "kp _attach: {}",
                        i18n::s("缺少 --socket", "missing --socket")
                    );
                    2
                }
            }
        }
        Some((a, rest)) if a == "_report" => match (rest.first(), rest.get(1)) {
            (Some(n), Some(c)) => client::cmd_report(n, c),
            _ => {
                eprintln!(
                    "kp _report: {}",
                    i18n::s(
                        "用法 kp _report <name> <code>",
                        "usage: kp _report <name> <code>"
                    )
                );
                2
            }
        },
        _ => client::cmd_run(cli.name, cli.policy, args),
    };
    std::process::exit(code);
}

fn parse_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
