# kp 范围文档

版本：0.1（草案）　日期：2026-11-01　上游：`01-requirements.md`

## 1. 里程碑总览

| 里程碑 | 主题 | 交付形态 | 预估（理想人日） |
|---|---|---|---|
| **M0 (v0.1)** | 走通核心闭环：启动/身份去重/attach/restart/多客户端/stop | 能替代日常 dev server 用法 | ~7 |
| **M1 (v0.2)** | 配置化 + 可运维：.kp.toml、wait/new/reject、logs -f、history、clean | 团队可共用（配置入库） | ~3.5 |
| **M2 (v0.3)** | 打磨：完成通知、fresh-env、readonly 体验、doctor、日志轮转完善 | 对外发布（brew/cargo） | ~3.5 |

## 2. FR × 里程碑映射

| FR | M0 | M1 | M2 |
|---|---|---|---|
| FR1 启动与接管 | ✅（含 `--`、退出码透传） | | banner 细节打磨 |
| FR2 身份 | ✅（hash + 默认忽略表 + `-n`） | env 快照落盘 | `--fresh-env` |
| FR3 策略 | attach、restart | wait、new、reject | |
| FR4 多客户端 | ✅ 多读单写（先到先得写锁） | | 只读 attach 的快捷入口 `kp watch` |
| FR5 Ctrl-C 语义 | ✅ | | |
| FR6 进程组清理 | ✅ | | |
| FR7 daemon | ✅（自动孵化/socket/协议握手） | | doctor 自检、孤儿认领预研 |
| FR8 日志 | ✅ 落盘 + 上限截断 | `logs -f` | 正规轮转 `.log.1` |
| FR9 管理子命令 | ls / attach / stop / status | restart / logs / history / clean | |
| FR10 配置 | | ✅ 两层配置 + 合并 + `config show` | `.kp.local.toml` |
| FR11 历史 | | ✅ | |
| FR12 通知 | | | ✅ |

## 3. 各里程碑完成定义（DoD）

**M0**：以下场景全部可演示且自动化测试覆盖——

1. 两个终端同目录跑同命令：第二个 attach 而非新起，横幅正确；
2. 关闭两个终端窗口，进程仍在跑，`kp ls` 可见，重跑原命令接回；
3. 任一 attach 端 Ctrl-C，进程退出，两端均回显退出码并退出；
4. `-p restart`：旧组（含孙进程）零残留退出后新实例可 attach；
5. `kp stop` 杀组零残留；`kp ls` 无 daemon 时不报错。

**M1**：`wait`/`reject` 策略行为正确；两层配置合并（内置<全局<项目<CLI）与 ignore_env 并集有测试；子目录启动命中同一项目根与身份；`kp logs -f` 可跟流；`kp history` 能查到退出码与时长。

**M2**：长任务完成触发系统通知；`restart --fresh-env` 生效；`kp doctor` 检查 tmux 版本/socket/状态目录；README、brew formula、cargo publish 就绪。

## 4. 明确不做（及理由）

| 不做 | 理由 |
|---|---|
| 多用户 / 跨账号共享 | 决策 D3；需要系统 daemon + ACL，范围失控 |
| Windows | ConPTY 全另一套；等有真实需求再说 |
| 自研 PTY 引擎 | tmux 已覆盖 v0 全部需求（Engine trait 保留后路），先验证身份去重这个核心创新 |
| 开机自启 / 崩溃自愈 | 与"手动守护"定位冲突，用户已有 launchd/systemd |
| 端口探活 / 健康检查 | restart 语义是"进程组退出即干净"，探活引入第二套状态机，收益存疑 |
| restart 时等待端口释放 | 同上；改为"等进程组完全退出再起新实例"即可覆盖绝大多数场景 |
| 远程 / SSH 共享会话 | 安全面扩大一个量级 |
| `.kp.toml` hooks / 任意脚本执行 | 避免恶意仓库通过配置影响执行（配置只声明 match/policy/name） |
| TUI 仪表盘 | `kp ls` 表格够用，警惕造 tmux copy |

## 5. 风险与对策

| 风险 | 等级 | 对策 |
|---|---|---|
| 身份 hash 误判（同串不同义 / 异义同串） | 高 | 默认忽略表覆盖 IDE 噪音；横幅兜底展示元信息；`-n` 显式逃生；`status` 展示 env 差异摘要 |
| 非幂等命令被默认 attach 蒙混 | 高 | 文档把 reject 写成 migrate 类命令的标准配法；横幅永远显示"attached 而非 started" |
| tmux 版本差异（macOS 自带 vs brew） | 中 | 启动时检测 ≥3.0，`kp doctor` 给指引 |
| 孙进程组外逃逸（daemonize 的服务脱离 pg） | 中 | 文档声明局限（同类工具共有）；注入 KP_SESSION_ID 便于排查 |
| daemon 崩溃 | 低 | tmux server 独立持有 PTY，会话存活；新 daemon 启动时 reconcile（sessions.json ↔ tmux 实况对账） |
| `&&`/管道被客户端 shell 吃掉 | 低 | README 高亮 + `--` 强约定 + 检测到裸 `&&` 时 stderr 警告 |
| 向上查找 `.kp.toml` 在嵌套项目误锚定 | 低 | 就近命中即停、不跨层合并祖先；`kp config show` 明示生效根与每层来源 |

## 6. 假设与依赖

- 使用者为本机单一 Unix 用户；tmux ≥ 3.0 由用户自备（kp 只提示不代装）；
- 状态目录 `~/.local/state/kp/`（日志/历史/会话注册表），配置目录 `~/.config/kp/`；
- 实现语言 Rust（clap4 + serde/toml + sha2 + std UnixListener），单二进制 `kp`；
- 系统架构、组件/时序/状态图（UML）见 `docs/05-architecture.md`。
- 仓库目录仍叫 `bkr` 只是历史路径；crates.io 短名 `kp` 已被一个休眠的 KeePass CLI 占用，故 cargo 包名用 `kp-cli`（未被占用），二进制名仍为 `kp`。
