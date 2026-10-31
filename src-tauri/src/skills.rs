// Skill / agent / command inventory and management (pause, resume, edit,
// open). Everything is plain files under ~/.claude:
//   skills   ~/.claude/skills/<name>/SKILL.md      paused → ~/.claude/skills.paused/<name>/
//   agents   ~/.claude/agents/<name>.md            paused → ~/.claude/agents.paused/<name>.md
//   commands ~/.claude/commands/<name>.md          paused → ~/.claude/commands.paused/<name>.md
//   plugin skills ~/.claude/plugins/cache/**/skills/<name>/SKILL.md (read-only)
// Pausing moves the item out of the folder Claude Code scans, so it stops
// loading its description into every session; resuming moves it back.
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Claude Code truncates/complains about skill descriptions beyond this.
pub const DESC_LIMIT: usize = 1024;

#[derive(Debug, Clone, Serialize)]
pub struct InvItem {
    pub name: String,
    pub description: String,
    /// "skill" | "agent" | "command"
    pub kind: String,
    /// "user" | "plugin"
    pub scope: String,
    pub plugin: String,
    pub enabled: bool,
    /// The markdown file (SKILL.md / <name>.md).
    pub file: String,
    /// The thing that moves on pause (skill folder or the .md file).
    pub root: String,
    pub editable: bool,
    #[serde(rename = "descLen")]
    pub desc_len: usize,
    pub warnings: Vec<String>,
}

fn claude_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude"))
}

/// Minimal YAML-frontmatter reader: top-level `key: value` pairs, with quoted
/// scalars and `>`/`|` block scalars. Enough for name/description.
pub fn frontmatter(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut lines = text.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return out;
    }
    let body: Vec<&str> = lines.take_while(|l| l.trim() != "---").collect();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        i += 1;
        if line.starts_with(' ') || line.starts_with('\t') || !line.contains(':') {
            continue;
        }
        let (k, v) = line.split_once(':').unwrap();
        let key = k.trim().to_string();
        let mut val = v.trim().to_string();
        let block = matches!(val.as_str(), ">" | "|" | ">-" | "|-" | ">+" | "|+");
        if block || val.is_empty() {
            let mut parts = Vec::new();
            while i < body.len() && (body[i].starts_with(' ') || body[i].starts_with('\t') || body[i].trim().is_empty()) {
                parts.push(body[i].trim());
                i += 1;
            }
            let sep = if val.starts_with('|') { "\n" } else { " " };
            val = parts.join(sep).trim().to_string();
        } else {
            // plain scalar continued on indented lines
            while i < body.len() && (body[i].starts_with("  ") || body[i].starts_with('\t')) {
                val.push(' ');
                val.push_str(body[i].trim());
                i += 1;
            }
            val = unquote(&val);
        }
        out.insert(key, val);
    }
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        // JSON string parsing covers YAML double-quoted escapes we care about
        return serde_json::from_str::<String>(s).unwrap_or_else(|_| s[1..s.len() - 1].to_string());
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

fn read_item(file: &Path, root: &Path, kind: &str, scope: &str, plugin: &str, enabled: bool) -> Option<InvItem> {
    let text = fs::read_to_string(file).ok()?;
    let fm = frontmatter(&text);
    let fallback = if kind == "skill" {
        root.file_name()?.to_string_lossy().to_string()
    } else {
        file.file_stem()?.to_string_lossy().to_string()
    };
    let name = fm.get("name").filter(|n| !n.is_empty()).cloned().unwrap_or(fallback);
    let description = fm.get("description").cloned().unwrap_or_default();
    Some(InvItem {
        desc_len: description.chars().count(),
        name,
        description,
        kind: kind.into(),
        scope: scope.into(),
        plugin: plugin.into(),
        enabled,
        file: file.to_string_lossy().into(),
        root: root.to_string_lossy().into(),
        editable: scope == "user",
        warnings: Vec::new(),
    })
}

fn scan_skill_dirs(dir: &Path, enabled: bool, out: &mut Vec<InvItem>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let md = p.join("SKILL.md");
        if p.is_dir() && md.exists() {
            if let Some(it) = read_item(&md, &p, "skill", "user", "", enabled) {
                out.push(it);
            }
        }
    }
}

fn scan_md_files(dir: &Path, kind: &str, enabled: bool, out: &mut Vec<InvItem>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_file() && p.extension().map(|x| x == "md").unwrap_or(false) {
            if let Some(it) = read_item(&p, &p, kind, "user", "", enabled) {
                out.push(it);
            }
        }
    }
}

/// Plugin skills live under ~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/skills/<name>/SKILL.md.
/// Several versions may be cached; keep the most recently modified per (plugin, name).
fn scan_plugins(base: &Path, out: &mut Vec<InvItem>) {
    let cache = base.join("plugins").join("cache");
    let mut best: HashMap<(String, String), (std::time::SystemTime, InvItem)> = HashMap::new();
    for e in WalkDir::new(&cache).max_depth(7).into_iter().filter_map(|e| e.ok()) {
        if e.file_name() != "SKILL.md" {
            continue;
        }
        let file = e.path();
        let Some(root) = file.parent() else { continue };
        if root.parent().and_then(|p| p.file_name()).map(|n| n != "skills").unwrap_or(true) {
            continue;
        }
        // plugin name = first path segment after cache/<marketplace>/
        let rel = file.strip_prefix(&cache).ok();
        let plugin = rel
            .and_then(|r| r.iter().nth(1))
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(item) = read_item(file, root, "skill", "plugin", &plugin, true) else { continue };
        let mtime = fs::metadata(file).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        let k = (plugin.clone(), item.name.clone());
        match best.get(&k) {
            Some((t, _)) if *t >= mtime => {}
            _ => {
                best.insert(k, (mtime, item));
            }
        }
    }
    out.extend(best.into_values().map(|(_, i)| i));
}

pub fn inventory() -> Vec<InvItem> {
    let mut out = Vec::new();
    let Some(base) = claude_dir() else { return out };
    scan_skill_dirs(&base.join("skills"), true, &mut out);
    scan_skill_dirs(&base.join("skills.paused"), false, &mut out);
    scan_md_files(&base.join("agents"), "agent", true, &mut out);
    scan_md_files(&base.join("agents.paused"), "agent", false, &mut out);
    scan_md_files(&base.join("commands"), "command", true, &mut out);
    scan_md_files(&base.join("commands.paused"), "command", false, &mut out);
    scan_plugins(&base, &mut out);

    // warnings
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for it in out.iter().filter(|i| i.enabled) {
        *seen.entry((it.kind.clone(), it.name.clone())).or_default() += 1;
    }
    for it in out.iter_mut() {
        if it.description.trim().is_empty() {
            it.warnings.push("No description — Claude can't tell when to use it".into());
        }
        if it.desc_len > DESC_LIMIT {
            it.warnings.push(format!("Description is {} chars (limit {})", it.desc_len, DESC_LIMIT));
        }
        if it.enabled && seen.get(&(it.kind.clone(), it.name.clone())).copied().unwrap_or(0) > 1 {
            it.warnings.push("Duplicate name — another enabled item uses it".into());
        }
    }
    out.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

/// Only paths inside ~/.claude may be touched by the actions below.
fn guard(path: &str) -> Result<PathBuf, String> {
    let base = claude_dir().ok_or("no home dir")?;
    let p = PathBuf::from(path);
    let canon = p.canonicalize().map_err(|e| format!("{path}: {e}"))?;
    let base = base.canonicalize().map_err(|e| e.to_string())?;
    if !canon.starts_with(&base) {
        return Err("refusing to touch a path outside ~/.claude".into());
    }
    Ok(canon)
}

/// Pause (enable=false) or resume (enable=true) a user skill/agent/command.
/// Returns the new root path.
pub fn set_enabled(root: &str, kind: &str, enable: bool) -> Result<String, String> {
    let src = guard(root)?;
    let base = claude_dir().ok_or("no home dir")?;
    let folder = match kind {
        "skill" => "skills",
        "agent" => "agents",
        "command" => "commands",
        _ => return Err(format!("unknown kind {kind}")),
    };
    let to_dir = if enable { base.join(folder) } else { base.join(format!("{folder}.paused")) };
    fs::create_dir_all(&to_dir).map_err(|e| e.to_string())?;
    let name = src.file_name().ok_or("bad path")?;
    let dst = to_dir.join(name);
    if dst.exists() {
        return Err(format!("{} already exists", dst.display()));
    }
    fs::rename(&src, &dst).map_err(|e| e.to_string())?;
    Ok(dst.to_string_lossy().into())
}

/// Replace the `description:` of a user item's frontmatter. The original file
/// is copied to ~/.claude/.tokenscope-backup first. The new value is written as
/// a JSON string, which is a valid YAML double-quoted scalar.
pub fn set_description(file: &str, text: &str) -> Result<(), String> {
    let p = guard(file)?;
    let orig = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = orig.lines().collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return Err("file has no frontmatter".into());
    }
    let end = lines.iter().skip(1).position(|l| l.trim() == "---").map(|i| i + 1).ok_or("unterminated frontmatter")?;
    let new_line = format!("description: {}", serde_json::to_string(text.trim()).unwrap());
    let mut out: Vec<String> = Vec::new();
    let mut i = 1;
    let mut replaced = false;
    out.push(lines[0].to_string());
    while i < end {
        let l = lines[i];
        if !replaced && l.starts_with("description:") {
            out.push(new_line.clone());
            replaced = true;
            i += 1;
            // drop the old value's continuation lines (indented / blank)
            while i < end && (lines[i].starts_with(' ') || lines[i].starts_with('\t') || lines[i].trim().is_empty()) {
                i += 1;
            }
            continue;
        }
        out.push(l.to_string());
        i += 1;
    }
    if !replaced {
        out.push(new_line);
    }
    for l in &lines[end..] {
        out.push(l.to_string());
    }
    let mut new_text = out.join("\n");
    if orig.ends_with('\n') {
        new_text.push('\n');
    }
    // backup
    if let Some(base) = claude_dir() {
        let bdir = base.join(".tokenscope-backup");
        let _ = fs::create_dir_all(&bdir);
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let tag = p.parent().and_then(|d| d.file_name()).map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let fname = format!("{stamp}-{tag}-{}", p.file_name().unwrap().to_string_lossy());
        fs::write(bdir.join(fname), &orig).map_err(|e| format!("backup failed: {e}"))?;
    }
    fs::write(&p, new_text).map_err(|e| e.to_string())
}

/// Open a file with the default app, or reveal it in Finder.
pub fn open_path(path: &str, reveal: bool) -> Result<(), String> {
    let p = guard(path)?;
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        if reveal {
            c.arg("-R");
        }
        c.arg(&p);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        if reveal {
            c.arg(format!("/select,{}", p.display()));
        } else {
            c.arg(&p);
        }
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let _ = reveal;
        let mut c = std::process::Command::new("xdg-open");
        c.arg(if p.is_file() { p.parent().unwrap_or(&p).to_path_buf() } else { p.clone() });
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_and_quoted() {
        let t = "---\nname: better-colors\ndescription: >\n  Color work in\n  web projects.\nother: x\n---\nbody";
        let fm = frontmatter(t);
        assert_eq!(fm["name"], "better-colors");
        assert_eq!(fm["description"], "Color work in web projects.");
        let t2 = "---\nname: a\ndescription: \"Say \\\"hi\\\" now\"\n---\n";
        assert_eq!(frontmatter(t2)["description"], "Say \"hi\" now");
    }
}
