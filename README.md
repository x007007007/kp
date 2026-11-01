# kp — keep processes running

`kp` 是一个透明的命令包装器：像 `nohup` 一样把命令交给后台，但保留完整的终端体验——随时重新接入、多窗口同时观看、重复命令自动去重。灵感来自 screen/tmux，但**不需要改变使用习惯**：你照常敲命令，只是在前面加了两个字母。

## 它解决什么问题

开发者总在多个终端 / IDE（VSCode、Trae、Cursor…）之间切换。长跑命令（dev server、watcher、长构建）绑死在启动它的终端上：

- 关掉终端 / 重启 IDE → 服务死了；
- 换个窗口再敲一遍同样的命令 → 起了第二个实例，端口冲突。

用 `kp`：

```console
# VSCode 里
$ kp make run server 127.0.0.1
─ kp ─────────────────────────────
 会话   kp-myproj-3f8a12c0b41d55e6（已启动）
 ...

# Trae 里再敲同一条命令 → 不新起，直接接入现有会话（只读）
$ kp make run server 127.0.0.1
─ kp ─────────────────────────────
 会话   kp-myproj-3f8a12c0b41d55e6（已接入·只读）
 ...
```

关掉任何一个窗口，会话继续跑；任一窗口 Ctrl-C，会话结束、所有窗口同步退出并回显退出码。

## 安装

```console
$ cargo install kp-cli
```

依赖 [tmux](https://github.com/tmux/tmux) ≥ 3.0（macOS: `brew install tmux`）。kp 使用独立 tmux socket（`-L kp`），与你自己的 tmux 会话完全隔离。

## 用法

```console
$ kp <命令...>              # 启动 / 按策略接入（核心用法）
$ kp -n NAME <命令...>      # 显式命名会话（默认 kp-<项目>-<NAME>）
$ kp -p restart <命令...>   # 重复时重启旧实例（改了代码之后）
```

管理：

```console
$ kp ls                     # 所有会话：状态 / PID / 时长 / 客户端数 / 分支
$ kp status <name>          # 单会话详情
$ kp attach <name>          # 接入既有会话
$ kp logs <name> [-f]       # 纯文本日志（attach 流之外的第二双眼睛）
$ kp restart <name> [--fresh-env]
$ kp stop <name> | -a       # 杀整个进程组（TERM → 10s → KILL）
$ kp history                # 已结束会话的退出码与时长
$ kp clean                  # 清理已退出会话与过期日志
$ kp config                 # 打印合并后的配置（排障用）
```

### 重复策略（`-p`）

| 策略 | 行为 | 典型场景 |
|---|---|---|
| `attach`（默认） | 接入现有会话 | dev server |
| `restart` | 杀旧进程组 → 起新实例 → 接入 | 代码已变更 |
| `wait` | 等现有会话退出后再执行 | 构建 / 测试排队 |
| `new` | 并行再起一个实例（名字加后缀） | 确需第二实例 |
| `reject` | 直接报错退出（exit 3） | db-migrate 等绝不能跑两遍的命令 |

### 会话身份

未显式命名时，身份 = `hash(命令参数 + 项目根 + 环境变量 − 忽略表)`。IDE 注入的噪音变量（`VSCODE_*`、`TRAE_*`、`TERM_PROGRAM` 等）已内置忽略；在 `.kp.toml` 里可追加。

## 配置：`.kp.toml`

放在项目根目录（向上查找，同 git），可提交进 git 团队共享；全局配置在 `~/.config/kp/config.toml`。合并链：内置默认 < 全局 < 项目 < 命令行。

```toml
[identity]
ignore_env = ["MY_NOISE_*"]     # 追加到内置忽略表

[run]
default_policy = "attach"
term_timeout   = 10             # 杀组时 TERM→KILL 的等待秒数
log_max_mb     = 50
history_keep   = 100

[[services]]
name   = "server"
match  = "make run server*"     # 对 argv 做 glob，自上而下首个命中生效
policy = "attach"

[[services]]
name   = "migrate"
match  = "make db-migrate*"
policy = "reject"               # 谁也别想跑第二遍
```

> 安全：仓库里的 `.kp.toml` 按不可信输入处理——它只声明匹配与策略，**不能**执行 hook 或任意脚本。

## 已知限制

- 复合命令（`&&`、管道）会被你的 shell 先解析——kp 只收到第一条命令。需要复合语义请显式包装：`kp sh -c 'a && b'`；
- 子进程若自行 `setsid` 脱离进程组，则不在整组杀灭范围内（同类工具共有）；
- 单用户设计：不支持多 Unix 账号共享会话。

## 开发

```console
$ git config core.hooksPath hooks   # 启用提交前秘密扫描
$ cargo test
$ scripts/check-secrets.sh --all    # 手动全量扫描
```

设计文档见 [`docs/`](docs/)（需求 / 范围 / WBS / 配置 / 架构）。

## License

[MIT](LICENSE)
