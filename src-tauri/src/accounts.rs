// Claude Code accounts = config dirs. Each `claude` login lives in its own dir
// (default ~/.claude, others via CLAUDE_CONFIG_DIR=~/.claude-work …), with its
// own projects/ logs, skills/, agents/ and plugins/.
//
// Discovery: ~/.claude ("Main") + every ~/.claude-<name> / ~/.claude.<name> dir
// that has a projects/ folder. An explicit list in
//   <data dir>/tokenscope/accounts.json   [{"name":"Work","dir":"~/.claude-work"}, …]
// replaces discovery when present.
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub name: String,
    /// Absolute config dir; also the account id stored on events.
    pub dir: String,
    /// true for ~/.claude (no CLAUDE_CONFIG_DIR needed to resume)
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    dir: String,
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

pub fn config_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("tokenscope").join("accounts.json"))
}

fn title(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn accounts() -> Vec<Account> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let default_dir = home.join(".claude");
    let mk = |name: String, dir: PathBuf| Account {
        is_default: dir == default_dir,
        dir: dir.to_string_lossy().into(),
        name,
    };

    if let Some(text) = config_path().and_then(|p| fs::read_to_string(p).ok()) {
        if let Ok(list) = serde_json::from_str::<Vec<Entry>>(&text) {
            let v: Vec<Account> = list
                .into_iter()
                .map(|e| mk(e.name, expand(&e.dir)))
                .filter(|a| PathBuf::from(&a.dir).is_dir())
                .collect();
            if !v.is_empty() {
                return v;
            }
        }
    }

    let mut out = vec![mk("Main".into(), default_dir.clone())];
    let mut extra: Vec<Account> = Vec::new();
    if let Ok(rd) = fs::read_dir(&home) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let suffix = fname
                .strip_prefix(".claude-")
                .or_else(|| fname.strip_prefix(".claude."))
                .or_else(|| fname.strip_prefix(".claude_"));
            let Some(suffix) = suffix else { continue };
            let p = e.path();
            if suffix.is_empty() || !p.is_dir() || !p.join("projects").is_dir() {
                continue;
            }
            extra.push(mk(title(suffix), p));
        }
    }
    extra.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(extra);
    out
}
