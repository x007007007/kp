use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::protocol::Policy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub branch: String,
    pub policy: Policy,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub exit_code: Option<i32>,
    /// exited | stopped | restart | session-gone | daemon-restart
    pub reason: String,
}

pub fn append(path: &Path, e: &HistoryEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(e)?)?;
    Ok(())
}

pub fn read(path: &Path) -> Vec<HistoryEntry> {
    let Ok(f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// 滚动清理：只保留最近 keep 条（FR11）。
pub fn prune(path: &Path, keep: usize) -> std::io::Result<()> {
    let all = read(path);
    if all.len() <= keep {
        return Ok(());
    }
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        for e in &all[all.len() - keep..] {
            writeln!(f, "{}", serde_json::to_string(e)?)?;
        }
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Policy;

    fn entry(name: &str, ended: u64) -> HistoryEntry {
        HistoryEntry {
            name: name.into(),
            argv: vec!["sleep".into()],
            cwd: "/tmp".into(),
            branch: "main".into(),
            policy: Policy::Attach,
            started_at_ms: ended - 100,
            ended_at_ms: ended,
            exit_code: Some(0),
            reason: "exited".into(),
        }
    }

    #[test]
    fn append_read_roundtrip() {
        let p = std::env::temp_dir().join(format!(
            "kp-hist-{}-{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        assert!(read(&p).is_empty());
        append(&p, &entry("a", 1_000)).unwrap();
        append(&p, &entry("b", 2_000)).unwrap();
        let all = read(&p);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "a");
        assert_eq!(all[1].ended_at_ms, 2_000);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn prune_keeps_latest() {
        let p = std::env::temp_dir().join(format!(
            "kp-prune-{}-{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        for i in 0..15 {
            append(&p, &entry(&format!("n{i}"), 1_000 + i)).unwrap();
        }
        prune(&p, 10).unwrap();
        let all = read(&p);
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].name, "n5", "应丢弃最老的 5 条");
        assert_eq!(all[9].name, "n14");
        // 已少于 keep：no-op
        prune(&p, 100).unwrap();
        assert_eq!(read(&p).len(), 10);
        let _ = std::fs::remove_file(p);
    }
}
