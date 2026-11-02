//! kp — keep processes running：透明命令包装 + 会话去重 + tmux 引擎。
//! 库 crate：`kp`（包装入口）与 `kpm`（管理入口）两个二进制共享。
//! 架构见 docs/05-architecture.md。

pub mod client;
pub mod config;
pub mod daemon;
pub mod engine;
pub mod glob;
pub mod history;
pub mod identity;
pub mod paths;
pub mod protocol;
pub mod registry;
pub mod spawn_cmd;
pub mod util;
