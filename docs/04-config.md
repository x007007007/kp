# kp 配置设计（全局配置 + 项目 .kp.toml）

版本：0.1（草案）　日期：2026-11-02　上游：`01-requirements.md` FR10
本文档是配置系统的**唯一权威**（schema、位置、发现、合并、校验）。

## 0. 设计原则

1. **薄**：只管四件事——身份怎么算、重复命令怎么办、进程怎么杀、日志留多少；
2. **两层同 schema**：全局与项目文件写法完全一致，项目层只是多一层"项目根锚定"语义；
3. **失败要响**：未知键 / 类型错 / 越界一律硬错误（exit 2）并指明文件与行号，绝不静默忽略；
4. **合并可解释**：`kpm config` 永远能回答"这个值从哪一层来"。

## 1. 文件位置与发现

| 层 | 路径 | 说明 |
|---|---|---|
| 全局 | `$XDG_CONFIG_HOME/kp/config.toml`（未设置时 `~/.config/kp/config.toml`） | 个人偏好；`--config <path>` 可整体替换该层（排障/测试用） |
| 项目 | 自 `realpath(cwd)` 逐级向上**第一个命中**的 `.kp.toml`，所在目录即 project_root；**就近命中即停**，不跨层合并祖先配置 | 可提交 git 团队共享；空文件合法（仅起锚定作用） |
| 项目本地（M2） | `project_root/.kp.local.toml` | 个人覆盖项目共享配置；约定 gitignore |

- 向上查找到 `/` 为止；未命中 `.kp.toml` → 无项目层，身份的 cwd 分量退回 `realpath(cwd)`（与 FR2 原行为一致）。
- 不做环境变量配置层（`KP_*` 仅保留运行时内部变量如 `KP_SESSION_ID`）：层数越少越易排查，临时覆盖用 CLI 参数即可。

## 2. 合并模型

合并链：**内置默认 < 全局 < 项目 < `.kp.local.toml`（M2） < CLI 参数**

| 数据形态 | 合并规则 |
|---|---|
| 标量键（`[run]` 全部、`[identity]` 非列表键） | 高层覆盖低层 |
| `identity.ignore_env` | **并集追加**（内置默认表 → 全局 → 项目），去重保序——它是排除名单，覆盖语义没有意义 |
| `[[services]]` | 合成一条候选链：**项目层在前、全局层在后**；同层按文件书写顺序；执行时**首个 match 命中生效** |
| `services.name` | 合并后的整条链上必须唯一，否则 exit 2（防止两条声明把不同命令映射到同一会话） |

配置解析发生在**客户端**：kp 以调用时的 cwd 语境解析全部配置，把解析后的会话参数（name/policy/term_timeout…）发给 daemon；daemon 不读配置文件——保持 cwd 无关、职责单一。

## 3. Schema 全量参考

```toml
# ============ [identity] 身份 ============
[identity]
ignore_env = []             # glob[]，追加到内置默认忽略表。例：["MY_NOISE_*", "CI_*"]

# ============ [run] 运行行为 ============
[run]
default_policy = "attach"   # attach | wait | restart | new | reject
                            # 未命中 service 且无 -p 时采用
engine        = "native"    # native（自研 PTY holder，默认，零依赖）| tmux（需 tmux ≥ 3.0）
                            # daemon 启动时由首个孵化它的客户端配置决定；切换引擎需重启 daemon
term_timeout   = 10         # 秒，1..=600；杀组时 TERM→KILL 的等待
log_max_mb     = 50         # 1..=2048；单会话日志上限
log_keep       = 1          # 轮转保留档数（M2 生效）
history_keep   = 100        # 10..=10000；history.jsonl 滚动条数
notify         = false      # 完成通知开关（M2 生效，对应 FR12）

# ============ [[services]] 服务声明（数组，自上而下首个命中生效） ============
[[services]]
name   = "server"           # 必填。slug：^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$
match  = "make run server*" # 必填。对 argv.join(" ") 做 glob（* 可跨词、? 单字符）
cwd    = "*"                # 可选。相对 project_root 的调用目录 glob；默认 "*"（任意目录）
policy = "attach"           # 可选。命中后的默认策略；缺省回落 run.default_policy
```

所有键在全局/项目两层均合法（schema 相同，无层独占键）。

### 3.1 services 匹配算法

1. 取 `argv.join(" ")`（kp 直接 exec argv、不经 shell，所见即所得）；
2. 沿候选链（项目层在前）逐条求值：`match` 对 argv 串、`cwd` 对"相对 project_root 的调用目录"，两者皆中才算命中；
3. 首个命中者胜出，采用其 name / policy；链尽无命中 → 散命令路径：hash 身份 + `run.default_policy`。

glob 引擎只实现 `*` 与 `?`，刻意不做 `[]` / `**`——匹配面越小，身份歧义越少。

### 3.2 与身份判定（FR2）的联动：project_root 锚定

- 原行为：身份的 cwd 分量 = `realpath(调用目录)`。缺陷：在 `~/proj` 与 `~/proj/packages/api` 各跑一次 `kp make dev` → hash 不同 → 去重失效。
- **新行为**：找到 `.kp.toml` 时，身份的 cwd 分量 = `realpath(project_root)`；调用目录只用于 `services.cwd` 匹配与 `kp status` 展示。效果：**任意子目录启动同一命令都命中同一会话**。
- 无 `.kp.toml` → 维持原行为；此时带 `cwd` 限定的 service（`cwd ≠ "*"`）因无锚定基准而跳过，`kpm config` 给出提示。
- 命中 service 的固定 `name` 直接作为身份（与 `-n` 等价），hash 不参与。

## 4. 示例

### 全局 `~/.config/kp/config.toml`

```toml
# 个人偏好，跨项目生效
[run]
term_timeout = 8

[identity]
ignore_env = ["*_API_KEY"]    # 各类密钥不参与身份

# 个人兜底规则；候选链里排在所有项目声明之后，项目层可覆盖
[[services]]
name   = "never-twice-migrate"
match  = "*migrate*"
policy = "reject"
```

### 项目 `~/proj/.kp.toml`

```toml
# 本项目共享，提交进 git
[run]
log_max_mb = 200              # 这个项目日志量大

[identity]
ignore_env = ["PNPM_*"]

[[services]]
name   = "server"
match  = "make run server*"
policy = "attach"             # 第二个窗口直接接上

[[services]]
name   = "migrate"
match  = "make db-migrate*"
policy = "reject"             # 谁也别想跑第二遍

# monorepo：同一命令在不同包是不同服务
[[services]]
name   = "web-dev"
cwd    = "apps/web*"
match  = "npm run dev"

[[services]]
name   = "api-dev"
cwd    = "apps/api*"
match  = "npm run dev"
```

## 5. 校验与错误

| 校验项 | 规则 | 失败行为 |
|---|---|---|
| 未知键 | 严格白名单 | exit 2 + 文件:行号 |
| 类型 | 不做隐式转换（`"10"` 不是 `10`） | exit 2 |
| 枚举 / 范围 | policy 五值；term_timeout 1..=600；log_max_mb 1..=2048；history_keep 10..=10000；notify 布尔 | exit 2 |
| name | slug 正则 + 合并链唯一 | exit 2 |
| match | 非空；仅允许 `*` `?` 两种元字符 | exit 2 |
| 文件状态 | 不存在 → 该层缺席（合法）；存在但不可读 / 解析失败 → **存在即必须可用** | exit 2 |

配置错误退出码定 2，与策略拒绝（reject = 3）区分。

## 6. 明确不进配置的能力

| 能力 | 理由 |
|---|---|
| hooks / 任意脚本执行 | 攻击面：恶意仓库可借 `.kp.toml` 执行代码 |
| autostart / 开机自启 | 与"手动守护"定位冲突（范围文档 §4） |
| env 注入（`[run].env`） | 与 env 快照 / restart 语义纠缠，进 backlog 观察 |
| per-service 运行参数（独立超时、独立日志额） | YAGNI；真出现需求时以 name 约定再议 |
| 环境变量配置层 | 层数越少越好排查；临时覆盖走 CLI |

## 7. 对既有文档的回写（随本文档同步）

- `01-requirements.md` FR2：新增"项目根锚定"条目；FR10：指向本文档、services 改为数组形式、补 `kpm config`；
- `02-scope.md` §2 FR10 行、§3 M1-DoD、§5 风险表同步两层合并与锚定风险；
- `03-wbs.md` WP7：任务细化（发现/合并/校验/config show），预估 1.0 → 1.5 人日。
