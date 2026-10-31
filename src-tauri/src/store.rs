// Incremental event store.
//
// Ingestion (this file) is the only place that touches the JSONL logs. It
// parses each assistant message into a provider/config/price-independent
// RawEvent (just the facts), reads only newly-appended bytes of changed files
// (tracked by a per-file size/mtime/offset manifest), dedupes by message id,
// and persists everything to the cache dir. Aggregation (parser.rs) then works
// purely on these in-memory events — cheap, and recomputed per request because
// the Day/Week/Month windows are relative to "now".
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Serialize, Deserialize, Clone)]
pub struct RawEvent {
    pub ts_ms: i64,
    pub session: String,
    pub model: String, // raw model id (price lookup), normalized later for grouping
    pub in_tok: f64,
    pub cc: f64, // cache creation
    pub cr: f64, // cache read
    pub out_tok: f64,
    pub mcp: Vec<String>,    // all mcp__<server> names called (unfiltered)
    pub skills: Vec<String>, // all Skill input.skill ids called (unfiltered)
    pub id: String,          // message id (dedup)
    // Source log file (manifest key). Lets a truncated/rewritten file purge its
    // own stale events before being re-read, so re-ingestion stays idempotent.
    #[serde(default)]
    pub source: String,
    // ── Keeper attribution facts (v5) ──
    // Every tool_use name in this message (built-ins like Edit/Bash and full
    // mcp__<server>__<tool> names), unfiltered. One entry per call.
    #[serde(default)]
    pub tools: Vec<String>,
    // Subagent type when the line came from a subagent transcript
    // (`<session>/subagents/agent-<id>.jsonl` + its .meta.json), else empty.
    #[serde(default)]
    pub agent: String,
    // promptId of the user turn this message belongs to (assistant lines don't
    // carry it, so it's inherited from the last user line in the same file).
    #[serde(default)]
    pub turn: String,
}

/// Per-session facts for the "hot sessions" list (cwd + title).
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct SessionInfo {
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub custom_title: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    // path -> (size, mtime_ms, byte offset already ingested)
    files: HashMap<String, (u64, i64, u64)>,
    // path -> last promptId seen (turn context carried across incremental reads)
    #[serde(default)]
    turns: HashMap<String, String>,
}

pub struct Store {
    pub events: Vec<RawEvent>,
    // message id -> index in `events`. A single assistant message can be split
    // across several JSONL lines (e.g. thinking on one line, tool_use on the
    // next) that all share its id; we merge their tool calls into one event and
    // count its token usage only once.
    index: HashMap<String, usize>,
    manifest: Manifest,
    pub sessions: HashMap<String, SessionInfo>,
}

// Bump when the parsing/extraction logic changes in a way that requires
// re-reading logs from scratch (the incremental manifest would otherwise skip
// already-seen bytes and miss newly-extracted facts).
//   v2: count slash-command skill invocations (`/skill`), not just Skill tool_use.
//   v3: merge tool_use across lines sharing a message id (a thinking line + a
//       tool_use line were deduped, dropping the tool call).
//   v4: track a per-event source file (idempotent re-read of truncated logs).
//   v5: keeper facts — every tool name, subagent type, turn (promptId), sessions.
const STORE_VERSION: u32 = 5;

/// Atomically replace `path`'s contents: write a sibling temp file, then rename
/// over the target (same-volume rename is atomic on Windows and Unix). Avoids
/// the half-written/truncated JSON that a crash mid-`fs::write` would leave.
fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

fn projects_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("projects"))
}

fn cache_dir() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&d);
    Some(d)
}

impl Store {
    /// Load persisted events + offset manifest (empty on first run).
    pub fn load() -> Self {
        let mut events: Vec<RawEvent> = Vec::new();
        let mut manifest = Manifest::default();
        let mut sessions: HashMap<String, SessionInfo> = HashMap::new();
        if let Some(dir) = cache_dir() {
            // If the cache was written by an older parser, discard it so ingest
            // does a full rescan and picks up newly-extracted facts.
            let version_ok = fs::read_to_string(dir.join("version"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                == Some(STORE_VERSION);
            if version_ok {
                // events.json and offsets.json are ONE consistent unit: the
                // manifest's per-file byte offsets are only meaningful relative
                // to the events we actually loaded. If either is missing or fails
                // to parse (e.g. a crash left events.json half-written), discard
                // BOTH and fall back to a full rescan — otherwise a good manifest
                // paired with empty/corrupt events would make ingest() skip every
                // already-recorded file and silently lose all history.
                let loaded_events = fs::read_to_string(dir.join("events.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Vec<RawEvent>>(&t).ok());
                let loaded_manifest = fs::read_to_string(dir.join("offsets.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Manifest>(&t).ok());
                if let (Some(e), Some(m)) = (loaded_events, loaded_manifest) {
                    events = e;
                    manifest = m;
                    sessions = fs::read_to_string(dir.join("sessions.json"))
                        .ok()
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or_default();
                }
            }
        }
        let index = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Store {
            events,
            index,
            manifest,
            sessions,
        }
    }

    pub fn save(&self) {
        if let Some(dir) = cache_dir() {
            // Atomic writes so a crash/kill mid-save can't leave a half-written
            // events.json (load() would then discard the pair and lose history).
            // Write events before offsets: if we crash between them, the manifest
            // is merely stale (points at fewer bytes → re-reads a little) rather
            // than ahead of the events on disk.
            if let Ok(t) = serde_json::to_string(&self.events) {
                let _ = write_atomic(&dir.join("events.json"), t.as_bytes());
            }
            if let Ok(t) = serde_json::to_string(&self.sessions) {
                let _ = write_atomic(&dir.join("sessions.json"), t.as_bytes());
            }
            if let Ok(t) = serde_json::to_string(&self.manifest) {
                let _ = write_atomic(&dir.join("offsets.json"), t.as_bytes());
            }
            let _ = write_atomic(&dir.join("version"), STORE_VERSION.to_string().as_bytes());
        }
    }

    /// Rebuild the id→index map after the `events` vector is mutated wholesale
    /// (purge/prune shift positions, so partial updates aren't enough).
    fn rebuild_index(&mut self) {
        self.index = self
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
    }

    /// Drop every event that came from `key`, then rebuild the index. Used before
    /// re-reading a truncated/rewritten file so re-ingestion is idempotent
    /// (otherwise the cross-line tool_use merge re-appends calls and id-less
    /// events get pushed twice, inflating MCP/Skill counts and token totals).
    fn purge_source(&mut self, key: &str) {
        self.events.retain(|e| e.source != key);
        self.rebuild_index();
    }

    /// Drop events older than `cutoff_ms`. The reports/heatmap only span the last
    /// ~26 weeks, so anything older is dead weight that grows events.json without
    /// bound. Returns whether anything was removed. Old logs already at EOF are
    /// never re-read, so their pruned events don't reappear.
    pub fn prune_before(&mut self, cutoff_ms: i64) -> bool {
        let before = self.events.len();
        self.events.retain(|e| e.ts_ms >= cutoff_ms);
        let removed = self.events.len() != before;
        if removed {
            self.rebuild_index();
        }
        removed
    }

    /// Incrementally read only the new bytes of new/changed JSONL files.
    /// Returns whether anything changed (new events or an updated file offset),
    /// so the caller can skip a full cache rewrite when nothing moved.
    pub fn ingest(&mut self) -> bool {
        let mut dirty = false;
        let Some(root) = projects_dir() else {
            return false;
        };
        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        {
            let path = entry.path();
            let key = path.to_string_lossy().to_string();
            let Ok(meta) = fs::metadata(path) else { continue };
            let size = meta.len();
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let mut offset = match self.manifest.files.get(&key).copied() {
                Some((psize, pmtime, poff)) => {
                    if psize == size && pmtime == mtime_ms {
                        continue; // unchanged → skip
                    }
                    if size < poff {
                        // truncated / rewritten (e.g. log compaction): the bytes
                        // we already ingested are gone, so purge this file's
                        // events and re-read from the start, idempotently.
                        self.purge_source(&key);
                        0
                    } else {
                        poff
                    }
                }
                None => 0,
            };

            let Ok(mut f) = fs::File::open(path) else { continue };
            if f.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            // only process up to the last newline; leave a partial trailing line
            // (file still being written) for the next pass
            let process_until = match buf.iter().rposition(|&b| b == b'\n') {
                Some(i) => i + 1,
                None => 0,
            };
            let agent = subagent_type(path);
            let mut cur_turn = self.manifest.turns.get(&key).cloned().unwrap_or_default();
            for line in buf[..process_until].split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let Ok(s) = std::str::from_utf8(line) else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else { continue };
                self.note_session(&v);
                let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty == "user" {
                    if let Some(p) = v.get("promptId").and_then(|p| p.as_str()) {
                        cur_turn = p.to_string();
                    }
                }
                if let Some(mut ev) = parse_value(&v) {
                    ev.source = key.clone();
                    ev.turn = cur_turn.clone();
                    ev.agent = if !agent.is_empty() {
                        agent.clone()
                    } else if v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true) {
                        "subagent".to_string()
                    } else {
                        String::new()
                    };
                    if !ev.id.is_empty() {
                        if let Some(&i) = self.index.get(&ev.id) {
                            // Same message, another line: merge its tool calls
                            // (don't re-count tokens — usage repeats per line).
                            let prev = &mut self.events[i];
                            prev.mcp.extend(ev.mcp);
                            prev.skills.extend(ev.skills);
                            prev.tools.extend(ev.tools);
                            continue;
                        }
                        self.index.insert(ev.id.clone(), self.events.len());
                    }
                    self.events.push(ev);
                }
            }
            if !cur_turn.is_empty() {
                self.manifest.turns.insert(key.clone(), cur_turn);
            }
            offset += process_until as u64;
            self.manifest.files.insert(key, (size, mtime_ms, offset));
            dirty = true;
        }
        dirty
    }
}

impl Store {
    /// Remember cwd / title per session (ai-title, custom-title lines).
    fn note_session(&mut self, v: &serde_json::Value) {
        let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) else { return };
        if sid.is_empty() {
            return;
        }
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let info = self.sessions.entry(sid.to_string()).or_default();
        if info.cwd.is_empty() {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                info.cwd = c.to_string();
            }
        }
        if ty == "custom-title" {
            if let Some(t) = v.get("customTitle").and_then(|t| t.as_str()) {
                info.title = t.to_string();
                info.custom_title = true;
            }
        } else if ty == "ai-title" && !info.custom_title {
            if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                info.title = t.to_string();
            }
        }
    }
}

/// For `<session>/subagents/agent-<id>.jsonl`, read the sibling
/// `agent-<id>.meta.json` → agentType ("general-purpose", "Explore", …).
fn subagent_type(path: &std::path::Path) -> String {
    let in_sub = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n == "subagents")
        .unwrap_or(false);
    if !in_sub {
        return String::new();
    }
    let meta = path.with_extension("meta.json");
    fs::read_to_string(meta)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("agentType").and_then(|a| a.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "subagent".to_string())
}

/// Parse one JSONL line into a RawEvent (assistant messages only).
#[allow(dead_code)]
fn parse_line(line: &str) -> Option<RawEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    parse_value(&v)
}

fn parse_value(v: &serde_json::Value) -> Option<RawEvent> {
    match v.get("type")?.as_str()? {
        "assistant" => parse_assistant(&v),
        // Skills invoked via slash command (e.g. `/find-skills`) are logged as a
        // user message with a <command-name> tag, NOT as a Skill tool_use, so
        // they need a separate path or they'd never be counted.
        "user" => parse_user_command(&v),
        _ => None,
    }
}

/// Extract the inner text of `<tag>...</tag>` from `s`, if present.
fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].to_string())
}

/// A user message that is a slash-command invocation of a skill, e.g.
/// `<command-name>/find-skills</command-name>`. The skill name is left
/// unfiltered here; compute_event drops non-user skills via the whitelist.
fn parse_user_command(v: &serde_json::Value) -> Option<RawEvent> {
    let text = v.get("message")?.get("content")?.as_str()?;
    let raw = extract_tag(text, "command-name")?;
    let skill = raw.trim().trim_start_matches('/').trim().to_string();
    if skill.is_empty() {
        return None;
    }
    let ts = v.get("timestamp")?.as_str()?;
    let ts_ms = DateTime::parse_from_rfc3339(ts).ok()?.timestamp_millis();
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    // dedup key: the line's own uuid (command messages have no message.id)
    let id = v.get("uuid").and_then(|i| i.as_str())?.to_string();
    if id.is_empty() {
        return None;
    }
    Some(RawEvent {
        ts_ms,
        session,
        model: String::new(), // not an LLM request → no model/tokens/cost
        in_tok: 0.0,
        cc: 0.0,
        cr: 0.0,
        out_tok: 0.0,
        mcp: Vec::new(),
        skills: vec![skill],
        id,
        source: String::new(),
        tools: Vec::new(),
        agent: String::new(),
        turn: String::new(),
    })
}

fn parse_assistant(v: &serde_json::Value) -> Option<RawEvent> {
    let msg = v.get("message")?;
    let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
    if model == "<synthetic>" {
        return None;
    }
    let ts = v.get("timestamp")?.as_str()?;
    let ts_ms = DateTime::parse_from_rfc3339(ts).ok()?.timestamp_millis();
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();

    let usage = msg.get("usage");
    let g = |k: &str| -> f64 {
        usage
            .and_then(|u| u.get(k))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };

    let mut mcp = Vec::new();
    let mut skills = Vec::new();
    let mut tools = Vec::new();
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if !name.is_empty() {
                tools.push(name.to_string());
            }
            if let Some(rest) = name.strip_prefix("mcp__") {
                mcp.push(rest.split("__").next().unwrap_or("").to_string());
            } else if name == "Skill" {
                if let Some(sk) = block
                    .get("input")
                    .and_then(|i| i.get("skill"))
                    .and_then(|s| s.as_str())
                {
                    if !sk.is_empty() {
                        skills.push(sk.to_string());
                    }
                }
            }
        }
    }

    Some(RawEvent {
        ts_ms,
        session,
        model: model.to_string(),
        in_tok: g("input_tokens"),
        cc: g("cache_creation_input_tokens"),
        cr: g("cache_read_input_tokens"),
        out_tok: g("output_tokens"),
        mcp,
        skills,
        id,
        source: String::new(),
        tools,
        agent: String::new(),
        turn: String::new(),
    })
}
