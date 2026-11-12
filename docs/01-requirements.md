# kp 需求文档

版本：0.1（草案）　日期：2026-11-01　状态：已评审通过的核心决策见 §7

## 1. 背景与问题

开发者在多个终端 / IDE（VSCode、Trae、Cursor、裸终端…）之间切换。长跑命令（dev server、watcher、tunnel、长构建）绑定在启动它的终端会话上：关掉终端 / 重启 IDE / 切窗口，进程就死或重启。tmux 能解决"进程别死"，但要求用户改变习惯——必须记得先开 tmux，且 attach 是手工按会话名操作，不认识"这是同一个服务"。

## 2. 目标与非目标

**目标**：用透明包装的方式（`kp <命令>`）让任意命令获得：

1. 进程生命周期与终端解耦（关终端/IDE 不死）；
2. 同一命令重复执行时按策略去重（attach / wait / restart / new / reject），而不是盲目再起一个；
3. 多窗口同时观察与操作（多读单写）；
4. 一条管理命令查看 / 控制所有在跑的会话。

**非目标（明确不做）**：

- 多用户（不同 Unix 账号）共享 —— 单用户多客户端；
- Windows（v1 只做 macOS / Linux，ConPTY 是另一套世界）；
- v0 不自研 PTY 引擎（tmux 作为引擎，见 §8）；
- 不做开机自启、远程/SSH 共享、进程健康探活（进 backlog，见范围文档）。

## 3. 核心概念与术语

| 术语 | 含义 |
|---|---|
| 会话 session | kp 管理的一个命令实例，有唯一名字、日志、退出码记录 |
| 身份 identity | 判定"两条命令是不是同一个逻辑服务"的依据（§5 FR2） |
| 策略 policy | 身份已存在时第二次执行的行为（§5 FR3） |
| daemon | 用户级后台进程，持有会话注册表，代客户端执行创建/杀灭/查询 |
| 客户端 client | `kp` 前端命令；attach 时是观察/操作窗口，非 attach 时是发指令的薄客户端 |
| 引擎 engine | 实际提供 PTY / attach 能力的层（v0 = tmux，藏在 trait 后） |

## 4. 用户场景

- **S1 跨 IDE 接管**：在 VSCode 的 `~/proj` 里跑 `kp make run server 127.0.0.1`；切到 Trae 在同一目录跑同一条命令 → 不新起，直接 attach 到现有进程，看到全部输出，两窗口同时可看。
- **S2 关窗不死**：关掉 VSCode 集成终端，server 继续跑；重开终端再跑同一条命令即接回。
- **S3 改码重启**：`kp -p restart make run server` → 杀掉旧实例整组进程，起新的。
- **S4 互斥命令防重**：`make db-migrate` 在 `.kp.toml` 里声明 `policy = "reject"` → 第二次执行直接报错并提示 `kp ls`。
- **S5 排队**：`kp -p wait make build` → 上一个同身份 build 没跑完时排队等它退出后再跑。
- **S6 巡检**：`kp ls` 看所有在跑的会话（名字/pid/时长/客户端数/git 分支），`kp logs -f server` 跟日志，`kp stop server` 杀掉，`kp history` 看已结束会话的退出码。

## 5. 功能需求

优先级：P0 = v0.1 必须；P1 = v0.2；P2 = v0.3 / backlog。

### FR1 命令启动与接管（P0）

`kp [-n NAME] [-p POLICY] [--] CMD...`

- **kp 没有子命令**（D10）：kp 之后的一切都是被包装的命令及其参数，`kp ls`/`kp help`/`kp restart` 都能包装同名系统命令；管理入口全部在 `kpm`（FR9）。
- 身份不存在 → 新建会话并在**前台 attach**（行为近似直接运行该命令，但进程活在引擎的 PTY 上）。
- 身份已存在 → 按 §FR3 策略处理。
- `--` 之后原样传参；含 `&&`/管道的复合命令需用户显式 `kp -- sh -c '...'`（文档必须显著说明此脚枪）。
- 退出码：会话进程的退出码透传给**所有** attach 中的客户端后，客户端以该码退出。
- 内部命令 `_spawn` / `_report` 供 tmux pane 与 hook 调用，不可被包装（fork bomb 防线）。

### FR2 会话身份（P0）

- 未指定 `-n` 时：`sha256( argv 规范化串 + realpath(cwd) + 排序后环境变量 − 忽略表 )` 取前 8 字节十六进制；会话名生成为 `kp-<项目目录名>-<hash8>`（引擎对会话名字符有约束，只允许 `[A-Za-z0-9_-]`）。
- 项目根锚定（详见 `docs/04-config.md` §3.2）：若自调用目录向上找到 `.kp.toml`，身份中的 cwd 分量改用该文件所在目录（project_root），任意子目录启动同一命令都命中同一会话；未找到时退回 `realpath(调用目录)`。调用目录另用于 services 的 cwd 匹配，并展示于 `kp status`。
- `-n NAME` 显式指定身份时，以 NAME 为准，不再做字符串归一。
- 内置默认忽略表（重点：IDE 注入的噪音变量，否则同一命令在 VSCode 和 Trae 里 hash 不同）：`SHLVL TERM TERM_PROGRAM TERM_PROGRAM_VERSION COLORTERM LANG LC_* COLUMNS LINES PWD OLDPWD _ TMUX STY SSH_TTY VSCODE_* TRAE_* CURSOR_* ITERM_* __CF_USER_TEXT_ENCODING XPC_*`；用户在配置 `identity.ignore_env` 里**追加**。
- 环境变量快照：spawn 时全量存档，`kp restart` 默认用快照，`--fresh-env` 用当前客户端环境重跑（P1）。

### FR3 重复策略（attach/restart P0；wait/new/reject P1）

| 策略 | 行为 | 典型用途 |
|---|---|---|
| `attach`（默认） | 前台接进现有会话 | dev server |
| `restart` | 优雅退出旧实例（TERM→超时→KILL）→ 起新实例 → attach | 代码已变 |
| `wait` | 阻塞等现有会话退出，然后正常执行 | 构建/测试 |
| `new` | 无视现有实例，并行再起一个（名字加后缀） | 双实例、端口冲突需自行负责并警告 |
| `reject` | 报错退出（exit code 3），提示 `kpm ls` | db-migrate 等互斥命令 |

优先级：CLI `-p` > 粘性已存（D11） > `.kp.toml` 中 service 声明 > 默认 `attach`。

- **粘性策略（D11）**：对某会话名用 `-p` 设置一次后，daemon 将生效策略持久化于 `policies.json`；此后对该服务的命令不带 `-p` 也沿用，直到再次 `-p` 改设。
- **重启不甩人**：restart（无论哪个窗口发起）时，其他已 attach 窗口以实例纪元（started_at_ms）判定并**自动跟随接入同名新实例**，提示"服务已重启，已重新接入新实例"。

### FR4 多客户端多读单写（P0）

- 同身份允许多个客户端同时 attach：第一个交互式客户端可写，其余**只读** attach。
- attach 时必须先打横幅（stderr）：会话名、pid、已运行时长、启动时 git 分支 + dirty 标记、当前客户端数、退出方式提示。横幅是对"静默魔法"的安全带。
- 客户端终端关闭 / 断连 ≠ 进程退出，只 detach。

### FR5 中断与退出语义（P0）

- 任意一个客户端 Ctrl-C → SIGINT 发给会话进程（引擎 PTY 前台组）→ 进程退出 → **所有**客户端回显退出码并退出。
- **只读客户端同样放行 Ctrl-C**（唯一例外）：多读单写约束的是观察者不得注入任意输入，不约束 FR5 的结束会话权利；其余键盘输入只读端一律不转发。
- 非交互启动（如脚本里）中进程收到 SIGINT/SIGTERM 同样触发全客户端退出流程。

### FR6 进程组清理（P0）

- 杀会话 = 杀整个进程组（负 pid），TERM → 默认 10s 超时 → SIGKILL；超时可配（`run.term_timeout`）。
- 不接受僵尸/孤儿：daemon 收割直接子进程；孙进程随组死亡。验收用"起一个会 fork 十个孙进程的脚本，stop 后 `ps` 验证零残留"。

### FR7 daemon（P0）

- 首次 `kp` 调用自动孵化（ssh-agent 模式）；unix socket：`$XDG_RUNTIME_DIR/kp/daemon.sock`，未设置时退回 `$TMPDIR`（macOS 用户级安全临时目录），socket 与目录权限 0600/0700。
- daemon 崩溃 → **会话存活**（tmux server 是独立守护进程，持有全部 PTY，不随 kp daemon 死亡）；新 daemon 启动时以 `sessions.json` + tmux `@kp_*` 会话选项对账认领（reconcile，机制见 `docs/05-architecture.md` §4.5）。spawn 时向子进程注入 `KP_SESSION_ID`，用于排查与孤儿归因。
- 单实例锁；`kp ls` 在 daemon 未运行时输出空列表而非报错。
- 协议：socket 上的长度前缀 JSON，含版本号握手；版本不匹配（二进制升级后旧 daemon 仍在跑）时**客户端自动重启 daemon** 并重试一次——会话由 tmux 承载不受影响，新 daemon reconcile 接管。

### FR8 日志（P0 文件 + P1 follow）

- 每会话一个纯文本日志文件（`~/.local/state/kp/logs/<session>.log`），由引擎 `pipe-pane` 流式落盘；attach 看到的实时流（含 ANSI）走引擎本身。
- 大小上限默认 50MB，超限轮转 `.log.1`（最多一个旧档）。
- `kpm logs <name> [-f] [-n LINES]`（P1 支持 `-f`）。

### FR9 管理命令（P0：ls/attach/stop/status；P1：restart/logs/history/clean/config）

管理入口是**独立的第二二进制 `kpm`**（D10，取代"单二进制子命令"的旧决策）：kp 本体零子命令以保住命令包装的命名空间，管理动作全部收敛到 kpm。

```
kpm ls                    # 表格：name / status / policy / pid / age / clients / branch / cmd
kpm status <name>         # 单会话详情：argv、cwd、env 差异摘要、快照时间
kpm attach <name>         # 等价于重复执行原命令
kpm stop <name> | -a      # 按 FR6 杀组
kpm restart <name> [--fresh-env]
kpm logs <name> [-f] [-n LINES]
kpm history               # 已结束会话：exit code、时长、原因（exited/stopped/restart）
kpm config                # 打印合并后配置、项目根与生效 services 链（配置排障）
kpm clean                 # 清死会话记录与过期日志
```

### FR10 配置文件（P1）

- 两层同 schema：项目 `.kp.toml`（可提交 git）+ 全局 `~/.config/kp/config.toml`；合并链：内置默认 < 全局 < 项目 < `.kp.local.toml`（M2）< CLI。解析在客户端完成，daemon 只收解析结果。
- 完整 schema、发现规则（`.kp.toml` 向上查找锚定项目根）、合并与校验语义的唯一权威：`docs/04-config.md`。
- 范围刻意收窄：身份忽略表、服务声明（match/cwd/name/policy）、运行参数。不做 hooks、不做自启、不做 env 注入。
- `kpm config`：打印合并结果、项目根、生效的 services 候选链——配置排障入口。

```toml
# 项目 .kp.toml 示例（摘录，完整 schema 见 docs/04-config.md）
[[services]]
name   = "server"
match  = "make run server*"     # 对 argv 串做 glob，候选链首个命中生效
policy = "attach"

[[services]]
name   = "migrate"
match  = "make db-migrate*"
policy = "reject"

[identity]
ignore_env = ["PNPM_*"]         # 追加到内置默认忽略表
```

### FR11 历史记录（P1）

`~/.local/state/kp/history.jsonl`：会话名、argv、cwd、开始/结束、退出码；`history_keep` 条滚动清理。

### FR12 完成通知（P2）

长任务完成时通过 OSC 9 / `notify-send` 发桌面通知（"kp: `make build` 退出码 0，用时 3m12s"）。

## 6. 非功能需求

- 单静态二进制（Rust），`cargo install` / brew 可装；macOS 10.15+ / 主流 Linux。
- attach 路径冷启动 < 100ms（不含引擎自身启动）。
- 客户端崩溃绝不影响会话进程；日志总量有上限（§FR8）。
- 许可 MIT；不收集任何数据。
- 引擎（D12）：默认 **native**（自研 PTY holder，无外部依赖）；`[run].engine = "tmux"` 时需 tmux ≥ 3.0（未安装时给出安装指引并退出）。

## 7. 决策记录（已拍板）

| # | 决策 | 备注 |
|---|---|---|
| D1 | 身份 = hash(命令串 + realpath(cwd) + 排序 env − 忽略表) | `-n` 显式优先 |
| D2 | 策略：attach / wait / restart，CLI 传参 | 本文档追加 new / reject |
| D3 | 单用户、多窗口；多读单写 | 多用户永久非目标（v1） |
| D4 | 任一客户端 Ctrl-C = 进程退出、全体客户端退出 | |
| D5 | 杀整组；TERM 默认 10s → KILL | |
| D6 | daemon：自动孵化 / 安全 socket / 崩溃后会话存活（tmux server 独立），新 daemon reconcile 认领 / 注入 KP_SESSION_ID | 修正于架构设计（tmux server 不随 daemon 死亡） |
| D7 | v0 引擎 = tmux（Engine trait 隔离） | screen 见 §8 |
| D8 | 名字 = `kp`（KeeP 缩写；原定 `keep` 因撞名与长度弃用）；`cargo install kp-cli` 产出 `kp` 与 `kpm` 两个二进制 | D10 修订"单二进制"决策 |
| D9 | 不考虑多用户、Windows v1 不做 | |
| D10 | kp 零子命令（纯包装），管理独立为 `kpm` | 解决 `kp ls`/`kp help` 等与系统命令的入口冲突 |
| D11 | 策略粘性：`-p` 设置一次即按会话名持久化（policies.json），之后默认沿用 | 优先级 CLI > 粘性 > .kp.toml > 默认 |
| D12 | 双引擎并存（v0.2）：**native PTY holder 为默认**（`[run].engine`，零外部依赖），tmux 保留为可选项；**screen 引擎否决**——无子进程退出码、无只读 attach、查询解析脆，原生引擎使其冗余 | trait 可扩展，未来如需 screen 为 drop-in |

## 8. 技术选型：tmux vs screen（引擎层）

| 维度 | tmux（选中） | GNU screen |
|---|---|---|
| 只读多客户端 | `attach -r` 原生 | `screen -x` 可多开，只读需 aclchg 绕行 |
| 脚本化能力 | `ls -F` 格式串、`send-keys`、`pipe-pane` 流式落盘、control mode | `stuff`、`hardcopy`（快照到临时文件）、`-Q`，解析脆 |
| 日志捕获 | pipe-pane 持续流 | `log on` 到 screenlog.N，可控性弱 |
| 维护 | OpenBSD 项目，活跃 | 长期维护模式 |
| 许可 | ISC/BSD | GPLv2+（子进程调用无传染问题，但 ISC 更省心） |

结论：两者都能实现 v0，tmux 的可编程面明显更强，**选 tmux**。引擎接口抽象为 `trait Engine`（create/attach/kill/ls/pipe_log），保留替换为 screen 或自研 PTY 的可能。

## 9. 名字

`kp`：**K**ee**P** 的缩写，"keep processes running"。两键即达——高频输入成本优先于可搜索性。撞名核查（2026-11-01）：本机无 `kp`/`keep` 命令与 alias；crates.io 的 `kp` 是一个 2021 年起未更新的 KeePass CLI，故 cargo 包名定为 `kp-cli`（未被占用），`[[bin]]` 产出二进制 `kp`，即 `cargo install kp-cli` 装出的命令就是 `kp`。注意事项：k8s 社区惯用 `kp` 作 `kubectl get pods` 的 alias，属 shell alias 层面冲突，README 提示用户自查 rc 文件。命名衍生：项目配置 `.kp.toml`、全局配置 `~/.config/kp/`、状态目录 `~/.local/state/kp/`、环境注入 `KP_SESSION_ID`、会话名前缀 `kp-`。前身 `keep` 因已有同名工具（PyPI keep / Keep）弃用；备选 `berth` / `roost` / `tether` 留档。
