//! 端到端集成测试：真实 daemon + client + holder 进程全链路（M0-DoD 自动化版）。
//! 每个测试独享一个临时 XDG 沙箱（经 Command env 传递，不污染测试进程全局环境）；
//! 真实 tmux 冒烟见 scripts/smoke-native.sh（含 pty 场景，CI 不便跑）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 串行化 e2e：Guard 清理用全局 pkill，并行跑会互杀对方的 daemon
static E2E_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn kp() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kp"))
}

fn kpm() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kpm"))
}

struct Env {
    root: PathBuf,
    work: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        // 用 /tmp 短前缀：macOS unix socket 路径上限 SUN_LEN=104，
        // std::env::temp_dir() 的 /var/folders/... 前缀过长会超限
        let root = PathBuf::from(format!(
            "/tmp/kpe-{}-{}-{}",
            tag,
            std::process::id(),
            &std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                .to_string()[8..]
        ));
        for d in ["state", "runtime", "config"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        Env { root, work }
    }

    fn apply(&self, cmd: &mut Command) {
        cmd.env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"));
    }

    fn sock(&self) -> PathBuf {
        self.root.join("runtime/kp/daemon.sock")
    }
}

/// 沙箱守卫：无论测试成败都清理 kp 进程（作用域限定在沙箱的 spec/state 路径）
struct Guard {
    root: PathBuf,
}

impl Guard {
    fn new(root: &Path) -> Self {
        Guard {
            root: root.to_path_buf(),
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = Command::new("pkill")
            .args(["-9", "-f", "kp --daemon"])
            .output();
        let _ = Command::new("pkill")
            .args(["-9", "-f", "kp _holder"])
            .output();
        let _ = Command::new("pkill")
            .args(["-9", "-f", "kp _attach"])
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run(bin: &Path, env: &Env, cwd: Option<&Path>, args: &[&str]) -> (i32, String) {
    let mut c = Command::new(bin);
    c.args(args);
    env.apply(&mut c);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let o = c.output().expect("命令执行失败");
    (
        o.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

/// 后台启动 kp（attach 会阻塞，等收尾时再收尸）
fn spawn_kp(env: &Env, cwd: &Path, args: &[&str], log: &Path) -> std::process::Child {
    let mut c = Command::new(kp());
    c.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(log).unwrap()))
        .stderr(Stdio::null());
    env.apply(&mut c);
    c.spawn().expect("启动 kp 失败")
}

fn wait_until(mut f: impl FnMut() -> bool, secs: u64) -> bool {
    let dl = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < dl {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    f()
}

fn ls(env: &Env) -> String {
    run(&kpm(), env, None, &["ls"]).1
}

fn first_running(env: &Env) -> Option<String> {
    for line in ls(env).lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 2 && cols[1] == "running" {
            return Some(cols[0].to_string());
        }
    }
    None
}

fn wait_running(env: &Env, secs: u64) -> Option<String> {
    wait_until(|| first_running(env).is_some(), secs);
    first_running(env)
}

#[test]
fn e2e_fast_exit_records_code() {
    let _g = lock();
    let env = Env::new("fastexit");
    let _guard = Guard::new(&env.root);
    // 回归：瞬间退出的命令，holder 的 ReportDone 曾可能先于注册表插入到达 daemon，
    // finalize 查不到条目即丢弃 → 退出码丢失、条目永远停留 running（幽灵会话）
    let (code, out) = run(&kp(), &env, Some(&env.work), &["echo", "hi"]);
    assert_eq!(code, 0, "kp echo hi 应以 0 退出: {out}");
    assert!(
        wait_until(|| ls(&env).contains("exit=0"), 8),
        "registry 应记录真实退出码 0，实际: {}",
        ls(&env)
    );
    // 连发多个瞬间退出会话，压缩竞态窗口
    for _ in 0..5 {
        let _ = run(&kp(), &env, Some(&env.work), &["sh", "-c", "true"]);
    }
    assert!(
        wait_until(|| !ls(&env).contains(" running "), 8),
        "不应有 running 残留: {}",
        ls(&env)
    );
}

#[test]
fn e2e_native_lifecycle() {
    let _serial = lock();
    let env = Env::new("native");
    let _g = Guard::new(&env.root);

    // 1. 显式启动 daemon
    let mut d = Command::new(kp());
    d.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            std::fs::File::create(env.root.join("daemon.log")).unwrap(),
        ))
        .stderr(Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(env.root.join("daemon.log"))
                .unwrap(),
        ));
    env.apply(&mut d);
    let mut daemon_child = d.spawn().expect("daemon 启动失败");
    let daemon_pid = daemon_child.id() as i32;
    assert!(
        wait_until(|| env.sock().exists(), 8),
        "daemon socket 未就绪；daemon.log: {}",
        std::fs::read_to_string(env.root.join("daemon.log")).unwrap_or_default()
    );

    // 2. 启动会话（native 引擎）→ running
    let mut c1 = spawn_kp(&env, &env.work, &["sleep", "60"], &env.root.join("c1.log"));
    let name = wait_running(&env, 10).expect("会话未进入 running");
    let l = ls(&env);
    assert!(l.contains("native"), "ENGINE 列应 native: {l}");

    // 3. 去重接入：第二个 client → 客户端数 2（c1 + c2），且没有第二个会话
    let mut c2 = spawn_kp(&env, &env.work, &["sleep", "60"], &env.root.join("c2.log"));
    assert!(
        wait_until(
            || {
                run(&kpm(), &env, None, &["status", &name])
                    .1
                    .contains("客户端 2 个")
            },
            8
        ),
        "去重接入后应为 2 个客户端"
    );
    assert_eq!(first_running(&env).as_deref(), Some(name.as_str()));

    // 4. stop → 会话消失，两个 client 退出
    let (code, o) = run(&kpm(), &env, None, &["stop", &name]);
    assert_eq!(code, 0, "stop 失败: {o}");
    assert!(wait_until(|| first_running(&env).is_none(), 8));
    let _ = c1.wait().unwrap();
    let _ = c2.wait().unwrap();

    // 5. daemon kill -9 → holder 存活 → kpm 触发孵化 + reconcile 认领
    let mut c3 = spawn_kp(&env, &env.work, &["sleep", "60"], &env.root.join("c3.log"));
    let name2 = wait_running(&env, 10).expect("第二个会话未 running");
    kill_pid(daemon_pid);
    // kpm 触发孵化新 daemon；holder 持有会话 → reconcile 应认领
    assert!(
        wait_until(
            || {
                let o = ls(&env);
                o.contains(&name2) && o.contains("running")
            },
            10
        ),
        "reconcile 未认领会话: {}",
        ls(&env)
    );

    // 6. 策略粘性：-p restart 创建后，kpm ls 显示 restart
    let (_c, o) = run(&kpm(), &env, None, &["stop", "--all"]);
    assert_eq!(_c, 0, "{o}");
    let mut r = Command::new(kp());
    // 短命令：client attach 后随会话自然退出，不拖慢测试
    r.args(["-p", "restart", "sleep", "3"])
        .current_dir(&env.work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.apply(&mut r);
    let mut c4 = r.spawn().unwrap();
    let name3 = wait_running(&env, 10).expect("restart 会话未 running");
    assert!(
        ls(&env).lines().any(|l| l.contains("restart")),
        "POLICY 列应 restart"
    );

    // 7. history 有记录
    assert!(wait_until(
        || {
            let o = run(&kpm(), &env, None, &["history"]).1;
            o.contains(&name2) || o.contains(&name3)
        },
        5
    ));

    let _ = c3.wait().unwrap();
    let _ = c4.wait().unwrap();
    // daemon 收尾：优雅终止并收割，避免僵尸
    kill_pid(daemon_pid);
    let _ = daemon_child.wait().unwrap();
}

#[test]
fn e2e_reject_policy_and_config() {
    let _serial = lock();
    let env = Env::new("reject");
    let _g = Guard::new(&env.root);

    // 项目配置：migrate 服务声明 reject
    std::fs::write(
        env.work.join(".kp.toml"),
        r#"[[services]]
name = "migrate"
match = "sleep *"
policy = "reject"
"#,
    )
    .unwrap();

    // daemon
    let mut d = Command::new(kp());
    d.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.apply(&mut d);
    let mut daemon_child = d.spawn().unwrap();
    assert!(wait_until(|| env.sock().exists(), 8));

    // 首次运行：正常启动（服务名会话 kp-<proj>-migrate）
    let mut c1 = spawn_kp(&env, &env.work, &["sleep", "30"], &env.root.join("r1.log"));
    assert!(
        wait_until(
            || first_running(&env)
                .map(|n| n.contains("migrate"))
                .unwrap_or(false),
            10
        ),
        "服务名会话应出现: {}",
        ls(&env)
    );

    // 重复运行：被 reject 拒绝，退出码 3
    let (code, o) = run(&kp(), &env, Some(&env.work), &["sleep", "30"]);
    assert_eq!(code, 3, "reject 应 exit 3: {o}");
    assert!(o.contains("reject"), "应提示 reject: {o}");

    // config 子命令：候选链可见
    let o = run(&kpm(), &env, Some(&env.work), &["config"]).1;
    assert!(
        o.contains("migrate") && o.contains("reject"),
        "config 应显示服务链: {o}"
    );

    let _ = c1.kill();
    let _ = c1.wait().unwrap();
    let _ = daemon_child.kill();
    let _ = daemon_child.wait().unwrap();
}

#[test]
fn e2e_tmux_engine_still_works() {
    let _serial = lock();
    if !tmux_available() {
        return; // CI 保证装有 tmux；本机缺失则跳过
    }
    let env = Env::new("tmux");
    let _g = Guard::new(&env.root);
    std::fs::write(env.work.join(".kp.toml"), "[run]\nengine = \"tmux\"\n").unwrap();

    let mut d = Command::new(kp());
    d.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.apply(&mut d);
    let mut daemon_child = d.spawn().unwrap();
    assert!(wait_until(|| env.sock().exists(), 8));

    let mut c1 = spawn_kp(&env, &env.work, &["sleep", "30"], &env.root.join("t1.log"));
    assert!(
        wait_until(|| first_running(&env).is_some(), 10),
        "tmux 引擎会话未 running: {}",
        ls(&env)
    );
    let l = ls(&env);
    assert!(l.contains("tmux"), "ENGINE 列应 tmux: {l}");
    let _ = c1.kill();
    let _ = c1.wait().unwrap();
    let _ = daemon_child.kill();
    let _ = daemon_child.wait().unwrap();
    // tmux server 清理（沙箱 socket 位于 temp runtime，随目录删除）
    let _ = Command::new("tmux")
        .args(["-L", "kp", "kill-server"])
        .output();
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn kill_pid(pid: i32) {
    unsafe {
        libc::kill(pid, 9);
    }
}
