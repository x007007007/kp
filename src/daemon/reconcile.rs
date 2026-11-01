//! daemon 启动对账（架构 §4.5）：以 tmux 实况为准恢复注册表。

use super::Ctx;
use crate::history::{self, HistoryEntry};
use crate::paths;
use crate::registry::{EntryState, Status};
use crate::util::now_ms;
use std::io::Read;

pub fn run(ctx: &Ctx) {
    let live = match ctx.eng.list() {
        Ok(l) => l,
        Err(_) => return,
    };
    let persisted = ctx.reg.snapshot();
    let mut live_names = Vec::new();

    for s in live {
        live_names.push(s.name.clone());
        let name = s.name.clone();
        let status_meta = ctx.eng.meta(&name, "@kp_status").unwrap_or(None);
        let spec_path = paths::specs_dir()
            .map(|d| d.join(format!("{name}.json")))
            .unwrap_or_default();
        let spec = read_spec_file(&spec_path).unwrap_or_else(|| minimal_spec(ctx, &name));

        match status_meta.as_deref() {
            Some("running") => {
                let info = ctx.eng.info(&name).ok().flatten();
                let pgid = info.as_ref().map(|i| i.pane_pid);
                let started = ctx
                    .eng
                    .meta(&name, "@kp_started")
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or_else(now_ms);
                let (status, exit_code, reason, ended) =
                    if info.as_ref().map(|i| i.pane_dead).unwrap_or(false) {
                        let code = info.as_ref().and_then(|i| i.pane_dead_status);
                        (Status::Exited, code, "exited".to_string(), Some(now_ms()))
                    } else {
                        (Status::Running, None, String::new(), None)
                    };
                ctx.reg.insert(
                    &name,
                    EntryState {
                        spec,
                        started_at_ms: started,
                        ended_at_ms: ended,
                        exit_code,
                        reason,
                        status,
                        pgid,
                        history_done: false,
                    },
                );
                if status == Status::Exited {
                    // daemon 死亡期间退出的会话：历史尚无记录，补一条
                    if let Some(e) = ctx.reg.get(&name) {
                        super::append_history_once(&e, None);
                    }
                    let _ = ctx.eng.kill_session(&name);
                }
            }
            _ => {
                // 无状态标记 / 已标记退出的残留会话 → 清理
                let _ = ctx.eng.kill_session(&name);
            }
        }
    }

    // sessions.json 有而 tmux 已无 → 补记历史后移除
    for (name, st) in persisted {
        if !live_names.contains(&name) {
            if !st.history_done {
                let Ok(hf) = paths::history_file() else {
                    continue;
                };
                let _ = history::append(
                    &hf,
                    &HistoryEntry {
                        name: st.spec.name.clone(),
                        argv: st.spec.argv.clone(),
                        cwd: st.spec.cwd.to_string_lossy().to_string(),
                        branch: st.spec.branch.clone(),
                        policy: st.spec.policy,
                        started_at_ms: st.started_at_ms,
                        ended_at_ms: st.ended_at_ms.unwrap_or_else(now_ms),
                        exit_code: st.exit_code,
                        reason: "daemon-restart".into(),
                    },
                );
            }
            ctx.reg.remove(&name);
        }
    }
    ctx.reg.persist();
}

fn read_spec_file(path: &std::path::Path) -> Option<crate::protocol::SessionSpec> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// spec 文件丢失时的降级重建（env 快照不可恢复，restart 需 --fresh-env）。
fn minimal_spec(ctx: &Ctx, name: &str) -> crate::protocol::SessionSpec {
    let meta = |k: &str| ctx.eng.meta(name, k).unwrap_or(None).unwrap_or_default();
    let argv: Vec<String> = meta("@kp_argv")
        .split('\u{1f}')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    crate::protocol::SessionSpec {
        name: name.to_string(),
        argv,
        cwd: std::path::PathBuf::from(meta("@kp_cwd")),
        env: Default::default(),
        policy: crate::protocol::Policy::Attach,
        log_path: paths::logs_dir()
            .map(|d| d.join(format!("{name}.log")))
            .unwrap_or_default(),
        spec_path: paths::specs_dir()
            .map(|d| d.join(format!("{name}.json")))
            .unwrap_or_default(),
        branch: meta("@kp_branch"),
        project_root: None,
        identity_hash: String::new(),
        term_timeout: 10,
        log_max_mb: 50,
    }
}
