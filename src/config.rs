use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::engine::EngineKind;
use crate::glob::glob_match;
use crate::paths;
use crate::protocol::Policy;

pub const PROJECT_FILE: &str = ".kp.toml";

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct FileCfg {
    identity: Option<IdentityCfg>,
    run: Option<RunCfg>,
    services: Option<Vec<ServiceCfg>>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct IdentityCfg {
    ignore_env: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct RunCfg {
    default_policy: Option<Policy>,
    term_timeout: Option<u64>,
    log_max_mb: Option<u64>,
    log_keep: Option<u32>,
    history_keep: Option<u32>,
    notify: Option<bool>,
    engine: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServiceCfg {
    pub name: String,
    #[serde(rename = "match")]
    pub match_: String,
    /// 相对 project_root 的调用目录 glob；空串 = "*"（04-config §3）。
    #[serde(default)]
    pub cwd: String,
    pub policy: Option<Policy>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Run {
    pub default_policy: Policy,
    /// 新会话使用的引擎（D12）：native | tmux
    pub engine: EngineKind,
    pub term_timeout: u64,
    pub log_max_mb: u64,
    pub log_keep: u32,
    pub history_keep: u32,
    pub notify: bool,
}

impl Default for Run {
    fn default() -> Self {
        Run {
            default_policy: Policy::Attach,
            engine: EngineKind::Native,
            term_timeout: 10,
            log_max_mb: 50,
            log_keep: 1,
            history_keep: 100,
            notify: false,
        }
    }
}

#[derive(Debug)]
pub struct Effective {
    pub ignore_env: Vec<String>,
    pub run: Run,
    pub services: Vec<ServiceCfg>,
    /// services 候选链中来自项目层的前 N 条（kp config 展示来源用）。
    pub project_services: usize,
    pub project_root: Option<PathBuf>,
    pub project_file: Option<PathBuf>,
    pub global_file: Option<PathBuf>,
}

/// 自 cwd 逐级向上找第一个 .kp.toml，就近命中即停（04-config §1）。
pub fn discover_project(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let f = d.join(PROJECT_FILE);
        if f.is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// 加载并合并配置。`global_override` 用于 --config 替换全局层（排障/测试）。
pub fn load(cwd: &Path, global_override: Option<&Path>) -> Result<Effective, String> {
    let global_path = match global_override {
        Some(p) => Some(p.to_path_buf()),
        None => paths::config_dir().ok().map(|d| d.join("config.toml")),
    };
    let gcfg = match &global_path {
        Some(p) if p.is_file() => Some(parse_file(p)?),
        _ => None,
    };
    let project_root = discover_project(cwd);
    let project_file = project_root.as_ref().map(|d| d.join(PROJECT_FILE));
    let pcfg = match &project_file {
        Some(p) => Some(parse_file(p)?),
        None => None,
    };

    // ignore_env：并集追加（内置 → 全局 → 项目）
    let mut ignore_env: Vec<String> = Vec::new();
    for c in [&gcfg, &pcfg].into_iter().flatten() {
        if let Some(list) = c.identity.as_ref().and_then(|i| i.ignore_env.as_ref()) {
            for v in list {
                if !ignore_env.contains(v) {
                    ignore_env.push(v.clone());
                }
            }
        }
    }

    // 标量：高层覆盖低层
    let mut run = Run::default();
    let mut cfg_engine: Option<String> = None;
    for c in [&gcfg, &pcfg].into_iter().flatten() {
        if let Some(r) = &c.run {
            if let Some(v) = r.default_policy {
                run.default_policy = v;
            }
            if let Some(v) = r.term_timeout {
                run.term_timeout = v;
            }
            if let Some(v) = r.log_max_mb {
                run.log_max_mb = v;
            }
            if let Some(v) = r.log_keep {
                run.log_keep = v;
            }
            if let Some(v) = r.history_keep {
                run.history_keep = v;
            }
            if let Some(v) = r.notify {
                run.notify = v;
            }
            if let Some(v) = &r.engine {
                cfg_engine = Some(v.clone());
                run.engine = EngineKind::parse(v).unwrap_or(run.engine);
            }
        }
    }

    // services 候选链：项目层在前、全局层在后（04-config §2）
    let mut services = Vec::new();
    if let Some(c) = &pcfg {
        services.extend(c.services.clone().unwrap_or_default());
    }
    let project_services = services.len();
    if let Some(c) = &gcfg {
        services.extend(c.services.clone().unwrap_or_default());
    }

    if let Some(e) = &cfg_engine {
        if EngineKind::parse(e).is_none() {
            return Err(t!("err.cfg_engine", e = e).to_string());
        }
    }
    validate(&services, &run)?;

    Ok(Effective {
        ignore_env,
        run,
        services,
        project_services,
        project_root,
        project_file,
        global_file: global_path,
    })
}

fn parse_file(p: &Path) -> Result<FileCfg, String> {
    let text = std::fs::read_to_string(p)
        .map_err(|e| t!("err.cfg_read", p = p.display(), e = e).to_string())?;
    toml::from_str(&text).map_err(|e| t!("err.cfg_parse", p = p.display(), e = e).to_string())
}

fn validate(services: &[ServiceCfg], run: &Run) -> Result<(), String> {
    let _ = run;
    if !(1..=600).contains(&run.term_timeout) {
        return Err(t!("err.cfg_term_timeout", v = run.term_timeout).to_string());
    }
    if !(1..=2048).contains(&run.log_max_mb) {
        return Err(t!("err.cfg_log_max", v = run.log_max_mb).to_string());
    }
    if !(10..=10000).contains(&run.history_keep) {
        return Err(t!("err.cfg_history_keep", v = run.history_keep).to_string());
    }
    let mut seen = HashSet::new();
    for s in services {
        if !valid_name(&s.name) {
            return Err(t!("err.cfg_service_name", name = s.name).to_string());
        }
        if s.match_.is_empty() {
            return Err(t!("err.cfg_service_match", name = s.name).to_string());
        }
        if !seen.insert(s.name.clone()) {
            return Err(t!("err.cfg_service_dup", name = s.name).to_string());
        }
    }
    Ok(())
}

pub fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    n.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// services 匹配：argv 串 glob + 可选调用目录 glob，首个命中生效。
/// cwd 限定在无 .kp.toml 锚定时跳过（04-config §3.2）。
pub fn resolve_service<'a>(
    cfg: &'a Effective,
    argv_str: &str,
    cwd: &Path,
) -> Option<&'a ServiceCfg> {
    for s in &cfg.services {
        if !glob_match(&s.match_, argv_str) {
            continue;
        }
        if s.cwd.is_empty() || s.cwd == "*" {
            return Some(s);
        }
        match &cfg.project_root {
            Some(root) => {
                let rel = cwd
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "..".into());
                if glob_match(&s.cwd, &rel) {
                    return Some(s);
                }
            }
            None => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kp-test-{tag}-{}-{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn merge_and_service_chain() {
        let proj = tmpdir("merge");
        let global = proj.join("global.toml");
        fs::write(
            &global,
            r#"
[run]
term_timeout = 8
[[services]]
name = "globalfallback"
match = "*migrate*"
policy = "reject"
"#,
        )
        .unwrap();
        let project = proj.join(PROJECT_FILE);
        fs::write(
            &project,
            r#"
[run]
term_timeout = 99
[identity]
ignore_env = ["PNPM_*"]
[[services]]
name = "server"
match = "make run server*"
policy = "attach"
"#,
        )
        .unwrap();

        let cfg = load(&proj, Some(&global)).unwrap();
        assert_eq!(cfg.project_root.as_deref(), Some(proj.as_path()));
        assert_eq!(cfg.run.term_timeout, 99, "项目层覆盖全局层");
        assert_eq!(cfg.ignore_env, vec!["PNPM_*".to_string()]);
        assert_eq!(cfg.services.len(), 2, "项目层在前、全局层在后");
        assert_eq!(cfg.services[0].name, "server");
        assert_eq!(cfg.services[1].name, "globalfallback");

        let hit = resolve_service(&cfg, "make run server 1.2.3.4", &proj).unwrap();
        assert_eq!(hit.name, "server");
        let hit2 = resolve_service(&cfg, "make db-migrate", &proj).unwrap();
        assert_eq!(hit2.name, "globalfallback");
        assert!(resolve_service(&cfg, "make build", &proj).is_none());
    }

    #[test]
    fn discovery_walks_up() {
        let root = tmpdir("walk");
        fs::write(root.join(PROJECT_FILE), "").unwrap();
        let deep = root.join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(discover_project(&deep).as_deref(), Some(root.as_path()));
        assert_eq!(discover_project(&tmpdir("none")), None);
    }

    #[test]
    fn cwd_scoped_service() {
        let root = tmpdir("cwdscope");
        fs::write(
            root.join(PROJECT_FILE),
            r#"
[[services]]
name = "web"
match = "npm run dev"
cwd = "apps/web*"
[[services]]
name = "api"
match = "npm run dev"
cwd = "apps/api*"
"#,
        )
        .unwrap();
        let cfg = load(&root, None).unwrap();
        let web = root.join("apps/web");
        fs::create_dir_all(&web).unwrap();
        let api = root.join("apps/api");
        fs::create_dir_all(&api).unwrap();
        assert_eq!(
            resolve_service(&cfg, "npm run dev", &web).unwrap().name,
            "web"
        );
        assert_eq!(
            resolve_service(&cfg, "npm run dev", &api).unwrap().name,
            "api"
        );
    }

    #[test]
    fn unknown_key_is_hard_error() {
        let d = tmpdir("badkey");
        let f = d.join("c.toml");
        fs::write(&f, "[run]\nwhatever = 1\n").unwrap();
        let err = load(&d, Some(&f)).unwrap_err();
        // 语言中立断言（i18n 后错误文案随环境变化；c.toml 路径两种语言都含）
        assert!(err.contains("c.toml"), "got: {err}");
    }

    #[test]
    fn duplicate_service_name_rejected() {
        let d = tmpdir("dupname");
        let f = d.join("c.toml");
        fs::write(
            &f,
            r#"
[[services]]
name = "a"
match = "x*"
[[services]]
name = "a"
match = "y*"
"#,
        )
        .unwrap();
        assert!(load(&d, Some(&f)).is_err());
    }

    #[test]
    fn name_validation() {
        assert!(valid_name("server"));
        assert!(valid_name("a-b_c9"));
        assert!(!valid_name("9好的"));
        assert!(!valid_name("-x"));
        assert!(!valid_name(""));
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kp-eng-{}-{}-{}",
            tag,
            std::process::id(),
            crate::util::now_ms()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn engine_valid_values() {
        let d = tmpdir("valid");
        let f = d.join("c.toml");
        fs::write(&f, "[run]\nengine = \"native\"\n").unwrap();
        assert_eq!(load(&d, Some(&f)).unwrap().run.engine, EngineKind::Native);
        fs::write(&f, "[run]\nengine = \"tmux\"\n").unwrap();
        assert_eq!(load(&d, Some(&f)).unwrap().run.engine, EngineKind::Tmux);
    }

    #[test]
    fn engine_invalid_is_hard_error() {
        let d = tmpdir("bad");
        let f = d.join("c.toml");
        fs::write(&f, "[run]\nengine = \"screen\"\n").unwrap();
        let err = load(&d, Some(&f)).unwrap_err();
        // 语言中立断言：run.engine 与取值表两种语言都含
        assert!(
            err.contains("run.engine") && err.contains("native"),
            "got: {err}"
        );
    }

    #[test]
    fn default_engine_is_native() {
        let d = tmpdir("def");
        assert_eq!(load(&d, None).unwrap().run.engine, EngineKind::Native);
    }
}
