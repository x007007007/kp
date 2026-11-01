# kp WBS（工作分解结构）

版本：0.1（草案）　日期：2026-11-01　上游：`01-requirements.md`、`02-scope.md`、`04-config.md`、`05-architecture.md`
预估单位：理想人日（ID 前缀 WP = Work Package）

## 1. 分解总表

| ID | 工作包 | 主要任务 | 产出物 | 预估 | 依赖 | 里程碑 |
|---|---|---|---|---|---|---|
| WP1 | 项目骨架 | cargo 初始化；clap4 CLI 骨架（所有子命令先占位）；CI（macOS/Linux 矩阵装 tmux）；fmt/clippy 门禁；README 骨架 | 可编译可装的单二进制 | 0.5 | — | M0 |
| WP2 | daemon 与协议 | socket 路径解析（XDG→TMPDIR）与 0600 权限；自动孵化 + 单实例锁；长度前缀 JSON 协议 + 版本握手；会话注册表（内存 + `sessions.json` 落盘）；启动 reconcile 对账（架构 §4.5） | daemon：spawn/ping/list/stop/wait RPC + janitor | 2.0 | WP1 | M0 |
| WP3 | 身份模块 | argv 规范化；环境变量排序 + 内置忽略表；sha256 → hash8；会话名 slug 化；`-n` 覆盖路径；env 快照结构体 | `identity.rs` 纯函数（无 IO，测试友好） | 1.0 | WP1 | M0 |
| WP4 | 策略与进程管理 | attach/restart 实现；`kp _spawn` 内部子命令（exec cmd、捕获退出码上报 daemon 后退出）；进程组 TERM→10s→KILL；`wait` 排队（M1）；`new`/`reject`（M1） | 策略引擎 + 退出码回传链路 | 1.5 | WP2 | M0 |
| WP5 | tmux 引擎适配 | `trait Engine` 定义；tmux 实现（一律专用 socket `tmux -L kp`）：new-session 跑 `kp _spawn`、pipe-pane 落日志、remain-on-exit + pane-died hook 退出上报、`@kp_*` 元数据选项、`list-* -F` 解析 | `engine/tmux.rs` | 1.5 | WP2 | M0 |
| WP6 | 客户端 UX | 前台 attach 流程；多读单写（查客户端数决定 `attach` vs `attach -r`）；横幅（name/pid/age/branch/clients）；Ctrl-C 透传与退出码回显；裸 `&&` 警告 | 端到端可用 | 1.0 | WP4, WP5 | M0 |
| WP7 | 配置系统 | 两层同 schema 解析；`.kp.toml` 向上查找锚定项目根（联动 FR2 身份）；合并链（内置<全局<项目<CLI；ignore_env 并集、services 候选链项目优先）；严格校验（未知键/类型/越界/重名 exit 2）；`kp config show` | `config.rs` + 发现/合并/校验测试 | 1.5 | WP3 | M1 |
| WP8 | 日志与历史 | logs 子命令（`-f` tail 实现）；大小上限截断（M2 轮转）；history.jsonl 写入与查询；clean | logs/history/clean | 1.0 | WP5 | M1 |
| WP9 | 打磨与发布 | 完成通知（OSC9/notify-send）；`restart --fresh-env`；`kp doctor`；README 场景文档；brew formula + cargo publish；screen 适配层可行性备忘 | v0.3 发布 | 2.0 | M1 全部 | M2 |
| — | 集成测试与加固 | 见 §2 测试策略，分散嵌入各 WP 验收，此处计总额 | 测试套件 | 1.5 | 各 WP | 跨里程碑 |

**合计：约 13.5 理想人日**（M0 ≈ 7.0，M1 ≈ 4.0，M2 ≈ 2.5~3.5，含缓冲建议按 1.5× 日历排期）。

## 2. 关键顺序与并行性

```
WP1 ─┬─ WP3 (身份, 纯函数, 可独立先行)
     └─ WP2 (daemon) ─┬─ WP4 (策略/进程)
                       └─ WP5 (tmux 引擎)   ← WP4 与 WP5 可并行
WP6 = WP4 + WP5 汇合  ← M0 端到端里程碑
WP7 依赖 WP3；WP8 依赖 WP5；两者可并行 → M1
WP9 收尾
```

## 3. 各 WP 验收标准（DoD）

- **WP1**：`kp --help` 全子命令可见；CI 两平台绿。
- **WP2**：杀掉 daemon 后首条命令自动拉起新 daemon；socket 权限实测 0600；版本不匹配报错清晰。
- **WP3**：同一命令 + 仅 IDE 噪音变量差异 → hash 相同；`PORT` 差异 → hash 不同（用例即需求 FR2）。
- **WP4**：`_spawn` 的退出码出现在所有客户端与 history；fork 10 个孙进程的脚本 stop 后 `pgrep` 零残留。
- **WP5**：tmux 不可用时错误信息含安装指引；pipe-pane 日志与 attach 所见内容一致（去 ANSI 后比对）。
- **WP6**：范围文档 §3 M0-DoD 的 5 条场景全部自动化。
- **WP7**：子目录启动命中同一项目根与身份；合并顺序与 ignore_env 并集有测试；未知键报错 exit 2；reject 声明 exit code = 3；`kp config show` 标注每层来源。
- **WP8**：50MB 上限生效；`logs -f` 在进程输出时 ≤1s 内可见。
- **WP9**：`cargo publish --dry-run` 通过；通知在 macOS 实测弹出。

## 4. 测试策略

1. **单元**：身份模块、配置合并、忽略表匹配（纯函数，覆盖 IDE 变量矩阵）。
2. **集成（真实 tmux）**：CI 里 `apt/brew install tmux`，脚本化模拟双终端（两个 pty 附件、断连、Ctrl-C 时序）。
3. **破坏性用例**：daemon 被 kill -9、tmux server 被杀、日志写满上限、会话名含非法字符。
4. **手工清单**：VSCode + Trae 双 IDE 真机走一遍 S1–S6（需求 §4），作为发版前检查单。

## 5. 待办提示（进入 WP 前需确认的遗留小项）

- `kp ls` 的表头字段最终排序（branch 放不放首列）—— M0 实拍后定；
- macOS 下 `$TMPDIR` socket 与 `$XDG_RUNTIME_DIR`（linux）路径统一封装在 WP2 一处实现；
- brew formula 归属个人 tap 还是 homebrew-core —— M2 再定。
