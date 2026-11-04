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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionSpec;
    use std::path::PathBuf;

    fn spec(name: &str) -> SessionSpec {
        SessionSpec {
            name: name.into(),
            argv: vec!["sleep".into(), "60".into()],
            cwd: PathBuf::from("/tmp"),
            env: Default::default(),
            policy: crate::protocol::Policy::Attach,
            policy_cli: None,
            engine: crate::engine::EngineKind::Native,
            log_path: PathBuf::from("/tmp/x.log"),
            spec_path: PathBuf::from("/tmp/x.json"),
            branch: "main".into(),
            project_root: None,
            identity_hash: "h".into(),
            term_timeout: 10,
            log_max_mb: 50,
        }
    }

    fn state(name: &str) -> EntryState {
        EntryState {
            spec: spec(name),
            started_at_ms: 1,
            ended_at_ms: None,
            exit_code: None,
            reason: String::new(),
            status: Status::Running,
            pgid: Some(42),
            history_done: false,
        }
    }

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kp-reg-{}-{}-{}",
            tag,
            std::process::id(),
            crate::util::now_ms()
        ))
    }

    #[test]
    fn insert_get_remove() {
        let r = Registry::load(tmp_path("mem"));
        assert!(r.get("a").is_none());
        r.insert("a", state("a"));
        assert!(r.get("a").is_some());
        let st = r.get("a").unwrap().st.lock().unwrap().clone();
        assert_eq!(st.status, Status::Running);
        assert_eq!(st.pgid, Some(42));
        r.remove("a");
        assert!(r.get("a").is_none());
    }

    #[test]
    fn persist_reload_roundtrip() {
        let path = tmp_path("disk");
        {
            let r = Registry::load(path.clone());
            r.insert("kp-b", state("kp-b"));
            r.insert("kp-a", {
                let mut s = state("kp-a");
                s.status = Status::Exited;
                s.exit_code = Some(130);
                s.ended_at_ms = Some(9);
                s
            });
            r.persist();
        }
        let r2 = Registry::load(path.clone());
        assert_eq!(r2.names(), vec!["kp-a".to_string(), "kp-b".to_string()]);
        let a = r2.get("kp-a").unwrap().st.lock().unwrap().clone();
        assert_eq!(a.status, Status::Exited);
        assert_eq!(a.exit_code, Some(130));
        assert_eq!(a.spec.engine, crate::engine::EngineKind::Native);
        assert!(r2.get("kp-b").is_some());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let r = Registry::load(tmp_path("missing"));
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn names_sorted() {
        let r = Registry::load(tmp_path("names"));
        r.insert("kp-z", state("kp-z"));
        r.insert("kp-a", state("kp-a"));
        assert_eq!(r.names(), vec!["kp-a".to_string(), "kp-z".to_string()]);
    }
}
