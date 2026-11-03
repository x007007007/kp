//! daemon 启动对账（架构 §4.5）：双引擎实况为准恢复注册表。
//! native 引擎的实况 = holder 状态文件；tmux 引擎的实况 = tmux -L kp 会话。

use super::Ctx;
use crate::engine::{EngineKind, EngineSessionInfo};
use crate::history::{self, HistoryEntry};
use crate::paths;
use crate::protocol::SessionSpec;
use crate::registry::{EntryState, Status};
use crate::util::now_ms;
use std::collections::HashMap;
use std::io::Read;

pub fn run(ctx: &Ctx) {
    // 两个引擎的 live 集合（名字互不重叠：注册表键唯一）
    let mut live: HashMap<String, (EngineKind, EngineSessionInfo)> = HashMap::new();
    for kind in [EngineKind::Native, EngineKind::Tmux] {
        if let Ok(list) = ctx.eng(kind).list() {
            for s in list {
                live.insert(s.name.clone(), (kind, s));
            }
        }
    }
    let persisted = ctx.reg.snapshot();
    let mut live_names = Vec::new();

    for (name, (kind, info)) in &live {
        live_names.push(name.clone());
        let spec_path = paths::specs_dir()
            .map(|d| d.join(format!("{name}.json")))
            .unwrap_or_default();
        let spec = read_spec_file(&spec_path).unwrap_or_else(|| minimal_spec(ctx, name, *kind));
        let mut spec = spec;
        spec.engine = *kind;

        let (status, exit_code, reason, ended) = if info.pane_dead {
            (
                Status::Exited,
                info.pane_dead_status,
                "exited".to_string(),
                Some(now_ms()),
            )
        } else {
            (Status::Running, None, String::new(), None)
        };
        ctx.reg.insert(
            name,
            EntryState {
                spec,
                started_at_ms: info_started(ctx, name, *kind).unwrap_or_else(now_ms),
                ended_at_ms: ended,
                exit_code,
                reason,
                status,
                pgid: Some(info.pane_pid),
                history_done: false,
            },
        );
        if status == Status::Exited {
            // daemon 死亡期间退出的会话：历史尚无记录，补一条
            if let Some(e) = ctx.reg.get(name) {
                super::append_history_once(&e, None);
            }
            let _ = ctx.eng(*kind).kill_session(name);
        }
    }

    // sessions.json 有而引擎实况已无 → 补记历史后移除
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

/// native 引擎可从状态文件拿到 started_at；tmux 用 @kp_started 选项。
fn info_started(ctx: &Ctx, name: &str, kind: EngineKind) -> Option<u64> {
    match kind {
        EngineKind::Native => {
            let p = crate::engine::native::session_status_path(name).ok()?;
            let text = std::fs::read_to_string(p).ok()?;
            serde_json::from_str::<crate::engine::native::HolderStatus>(&text)
                .ok()
                .map(|s| s.started_at_ms)
        }
        EngineKind::Tmux => ctx
            .eng(kind)
            .meta(name, "@kp_started")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok()),
    }
}

fn read_spec_file(path: &std::path::Path) -> Option<SessionSpec> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// spec 文件丢失时的降级重建（env 快照不可恢复，restart 需 --fresh-env）。
fn minimal_spec(ctx: &Ctx, name: &str, kind: EngineKind) -> SessionSpec {
    let meta = |k: &str| {
        ctx.eng(kind)
            .meta(name, k)
            .unwrap_or(None)
            .unwrap_or_default()
    };
    let argv: Vec<String> = meta("@kp_argv")
        .split('\u{1f}')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    SessionSpec {
        name: name.to_string(),
        argv,
        cwd: std::path::PathBuf::from(meta("@kp_cwd")),
        env: Default::default(),
        policy: crate::protocol::Policy::Attach,
        policy_cli: None,
        engine: kind,
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
