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
