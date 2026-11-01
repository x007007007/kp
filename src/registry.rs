use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use crate::protocol::SessionSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Exited,
}

#[derive(Debug, Clone)]
pub struct EntryState {
    pub spec: SessionSpec,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub reason: String,
    pub status: Status,
    pub pgid: Option<i32>,
    /// 历史只入账一次（_spawn 主动上报 / stop/restart / hook 多路径幂等）。
    pub history_done: bool,
}

/// 每会话一把锁 + 一个条件变量（WaitExit 长轮询用）。
pub struct Entry {
    pub st: Mutex<EntryState>,
    pub cv: Condvar,
}

#[derive(Serialize, Deserialize, Default)]
struct Persisted {
    sessions: HashMap<String, PersistEntry>,
}

#[derive(Serialize, Deserialize)]
struct PersistEntry {
    spec: SessionSpec,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    exit_code: Option<i32>,
    reason: String,
    status: Status,
    pgid: Option<i32>,
    #[serde(default)]
    history_done: bool,
}

/// 会话注册表：内存权威 + sessions.json 落盘缓存（reconcile 以 tmux 实况为准，架构 A6）。
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    path: Arc<PathBuf>,
}

impl Registry {
    pub fn load(path: PathBuf) -> Self {
        let mut map: HashMap<String, Arc<Entry>> = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(p) = serde_json::from_str::<Persisted>(&text) {
                for (name, e) in p.sessions {
                    map.insert(
                        name,
                        Arc::new(Entry {
                            st: Mutex::new(EntryState {
                                spec: e.spec,
                                started_at_ms: e.started_at_ms,
                                ended_at_ms: e.ended_at_ms,
                                exit_code: e.exit_code,
                                reason: e.reason,
                                status: e.status,
                                pgid: e.pgid,
                                history_done: e.history_done,
                            }),
                            cv: Condvar::new(),
                        }),
                    );
                }
            }
        }
        Registry {
            inner: Arc::new(Mutex::new(map)),
            path: Arc::new(path),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<Entry>> {
        self.inner.lock().unwrap().get(name).cloned()
    }

    pub fn insert(&self, name: &str, state: EntryState) {
        self.inner.lock().unwrap().insert(
            name.to_string(),
            Arc::new(Entry {
                st: Mutex::new(state),
                cv: Condvar::new(),
            }),
        );
    }

    pub fn remove(&self, name: &str) -> Option<Arc<Entry>> {
        self.inner.lock().unwrap().remove(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn snapshot(&self) -> Vec<(String, EntryState)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, e)| (k.clone(), e.st.lock().unwrap().clone()))
            .collect()
    }

    /// 原子落盘 sessions.json（架构 §8）。
    pub fn persist(&self) {
        let map = self.inner.lock().unwrap();
        let mut p = Persisted::default();
        for (name, e) in map.iter() {
            let st = e.st.lock().unwrap();
            p.sessions.insert(
                name.clone(),
                PersistEntry {
                    spec: st.spec.clone(),
                    started_at_ms: st.started_at_ms,
                    ended_at_ms: st.ended_at_ms,
                    exit_code: st.exit_code,
                    reason: st.reason.clone(),
                    status: st.status,
                    pgid: st.pgid,
                    history_done: st.history_done,
                },
            );
        }
        drop(map);
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_string_pretty(&p) {
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &*self.path);
            }
        }
    }
}
