// "Keeper" view: where the tokens went, per skill / agent / tool / MCP server
// / slash command, over rolling 5h / 24h / 7d / 30d windows, plus the list of
// currently hot sessions.
//
// Attribution is an estimate — the API reports usage per assistant message,
// not per tool. Rules (documented in the UI tooltip too):
//   • tool / MCP: a message's tokens are split evenly across the tool calls it
//     makes (Edit+Bash in one message → half each).
//   • agent: every message of a subagent transcript goes to its agent type.
//   • skill / command: from the moment a skill fires until the end of that user
//     turn, every message's tokens go to the skill(s) active in the turn.
// A message can therefore count toward a tool, a skill and an agent at once —
// rows overlap by design and don't sum to the total.
use crate::pricing::Pricing;
use crate::store::{RawEvent, SessionInfo};
use chrono::{DateTime, Local, TimeZone};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
pub struct KeeperItem {
    pub name: String,
    /// "skill" | "agent" | "tool" | "mcp" | "command"
    pub kind: String,
    pub tokens: f64, // raw tokens (all four types)
    pub cost: f64,
    pub count: u64,
    #[serde(rename = "lastMs")]
    pub last_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeeperWindow {
    pub key: String,
    #[serde(rename = "totalTokens")]
    pub total_tokens: f64,
    #[serde(rename = "totalCost")]
    pub total_cost: f64,
    pub requests: u64,
    pub sessions: u64,
    pub bars: Vec<f64>,
    #[serde(rename = "barLabels")]
    pub bar_labels: Vec<String>,
    pub items: Vec<KeeperItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HotSession {
    pub session: String,
    pub project: String,
    pub cwd: String,
    pub title: String,
    /// current context size = last main-thread request's input+cache tokens
    pub context: f64,
    #[serde(rename = "tokens5h")]
    pub tokens_5h: f64,
    #[serde(rename = "cost5h")]
    pub cost_5h: f64,
    #[serde(rename = "lastMs")]
    pub last_ms: i64,
    pub hot: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Keeper {
    pub windows: Vec<KeeperWindow>,
    pub sessions: Vec<HotSession>,
    pub alert: bool,
    /// skill/agent/command name → last time it ran (any window, up to the store horizon)
    #[serde(rename = "lastUsed")]
    pub last_used: HashMap<String, i64>,
    /// name → runs in the last 30 days (for "never runs" flags)
    #[serde(rename = "runs30d")]
    pub runs_30d: HashMap<String, u64>,
}

/// Thresholds for the "expensive session" alert.
pub const HOT_CONTEXT: f64 = 300_000.0; // tokens in the live context window
pub const HOT_5H: f64 = 150_000_000.0; // tokens burned by one session in 5h
const ACTIVE_MS: i64 = 30 * 60 * 1000; // a session counts as live for 30 min

/// Slash commands built into Claude Code — not user skills, not worth a row.
const BUILTIN_COMMANDS: &[&str] = &[
    "add-dir", "agents", "bug", "btw", "clear", "compact", "config", "context", "cost", "doctor",
    "effort", "exit", "export", "fast", "help", "hooks", "ide", "init", "install-github-app",
    "login", "logout", "mcp", "memory", "model", "output-style", "permissions", "plugin",
    "plugins", "privacy-settings", "release-notes", "resume", "rewind", "status", "statusline",
    "terminal-setup", "todos", "upgrade", "usage", "vim", "theme", "feedback", "remote-control",
];

/// Tool names that are really a skill/agent dispatch — accounted elsewhere.
const DISPATCH_TOOLS: &[&str] = &["Skill", "Agent", "Task"];

struct Attr {
    name: String,
    kind: &'static str,
    share: f64, // fraction of the message's tokens
    calls: u64, // invocation count contributed
}

fn total(e: &RawEvent) -> f64 {
    e.in_tok + e.cc + e.cr + e.out_tok
}

fn short_skill(s: &str) -> String {
    s.rsplit(':').next().unwrap_or(s).to_string()
}

pub fn build(events: &[RawEvent], pricing: &Pricing, sessions: &HashMap<String, SessionInfo>, user_commands: &HashSet<String>) -> Keeper {
    let now = Local::now();
    let now_ms = now.timestamp_millis();
    let horizon = now_ms - 31 * 86_400_000;

    // ── skill activations per (source file, turn) ──
    // value: (ts, skill name, kind) sorted by ts
    let mut turn_skills: HashMap<(&str, &str), Vec<(i64, String, &'static str)>> = HashMap::new();
    let mut last_used: HashMap<String, i64> = HashMap::new();
    for e in events {
        if !e.agent.is_empty() && e.agent != "subagent" {
            let l = last_used.entry(e.agent.clone()).or_default();
            *l = (*l).max(e.ts_ms);
        }
        if e.skills.is_empty() {
            continue;
        }
        let slash = e.model.is_empty(); // command lines carry no model
        for s in &e.skills {
            let name = short_skill(s);
            let kind: &'static str = if !slash {
                "skill"
            } else if BUILTIN_COMMANDS.contains(&name.as_str()) {
                continue;
            } else if user_commands.contains(&name) {
                "command"
            } else {
                // `/skill-name` typed by hand also invokes the skill
                "skill"
            };
            let l = last_used.entry(name.clone()).or_default();
            *l = (*l).max(e.ts_ms);
            if !e.turn.is_empty() {
                turn_skills
                    .entry((e.source.as_str(), e.turn.as_str()))
                    .or_default()
                    .push((e.ts_ms, name, kind));
            }
        }
    }
    for v in turn_skills.values_mut() {
        v.sort_by_key(|x| x.0);
    }

    // ── per-event attribution (window-independent) ──
    let mut attrs: Vec<(usize, f64, f64, Vec<Attr>)> = Vec::new(); // (event idx, tokens, cost, attrs)
    for (i, e) in events.iter().enumerate() {
        if e.ts_ms < horizon {
            continue;
        }
        let tok = total(e);
        let cost = if e.model.is_empty() {
            0.0
        } else {
            pricing
                .cost(&e.model, e.in_tok, e.out_tok, e.cc, e.cr)
                .or_else(|| pricing.cost(&crate::parser::normalize_model(&e.model), e.in_tok, e.out_tok, e.cc, e.cr))
                .unwrap_or(0.0)
        };
        let mut a: Vec<Attr> = Vec::new();

        // tools / MCP
        let calls: Vec<&String> = e.tools.iter().filter(|t| !DISPATCH_TOOLS.contains(&t.as_str())).collect();
        if !calls.is_empty() {
            let share = 1.0 / calls.len() as f64;
            let mut grouped: HashMap<(String, &'static str), (f64, u64)> = HashMap::new();
            for t in calls {
                let (name, kind) = match t.strip_prefix("mcp__") {
                    Some(rest) => (rest.split("__").next().unwrap_or(rest).to_string(), "mcp"),
                    None => (t.clone(), "tool"),
                };
                let g = grouped.entry((name, kind)).or_default();
                g.0 += share;
                g.1 += 1;
            }
            for ((name, kind), (share, n)) in grouped {
                a.push(Attr { name, kind, share, calls: n });
            }
        }

        // agent
        if !e.agent.is_empty() {
            a.push(Attr { name: e.agent.clone(), kind: "agent", share: 1.0, calls: 0 });
        }

        // skills active in this turn (fired at or before this message)
        if let Some(list) = turn_skills.get(&(e.source.as_str(), e.turn.as_str())) {
            let mut active: Vec<(&String, &'static str)> = Vec::new();
            for (ts, name, kind) in list {
                if *ts <= e.ts_ms && !active.iter().any(|(n, _)| *n == name) {
                    active.push((name, *kind));
                }
            }
            if !active.is_empty() {
                let share = 1.0 / active.len() as f64;
                for (name, kind) in active {
                    a.push(Attr { name: name.clone(), kind, share, calls: 0 });
                }
            }
        }
        // invocation counts for skills fired *by* this event
        for s in &e.skills {
            let name = short_skill(s);
            if let Some(x) = a.iter_mut().find(|x| x.name == name && (x.kind == "skill" || x.kind == "command")) {
                x.calls += 1;
            } else if !(e.model.is_empty() && BUILTIN_COMMANDS.contains(&name.as_str())) {
                let kind = if e.model.is_empty() && user_commands.contains(&name) { "command" } else { "skill" };
                a.push(Attr { name, kind, share: 0.0, calls: 1 });
            }
        }
        // agent launches: count on the parent's Agent/Task call
        let launches = e.tools.iter().filter(|t| *t == "Agent" || *t == "Task").count() as u64;
        if launches > 0 {
            a.push(Attr { name: "__agent_launch".into(), kind: "agent", share: 0.0, calls: launches });
        }
        attrs.push((i, tok, cost, a));
    }

    // ── windows ──
    let specs: [(&str, i64, usize); 4] = [
        ("5h", 5 * 3_600_000, 30),
        ("24h", 24 * 3_600_000, 24),
        ("week", 7 * 86_400_000, 28),
        ("month", 30 * 86_400_000, 30),
    ];
    let mut windows = Vec::new();
    let mut runs_30d: HashMap<String, u64> = HashMap::new();
    for (key, span, nbars) in specs {
        let start = now_ms - span;
        let bucket = span as f64 / nbars as f64;
        let mut bars = vec![0.0; nbars];
        let mut items: HashMap<(String, &'static str), KeeperItem> = HashMap::new();
        let (mut tt, mut tc, mut req) = (0.0, 0.0, 0u64);
        let mut sess: HashSet<&str> = HashSet::new();
        for (i, tok, cost, a) in &attrs {
            let e = &events[*i];
            if e.ts_ms < start || e.ts_ms > now_ms {
                continue;
            }
            tt += tok;
            tc += cost;
            if !e.model.is_empty() {
                req += 1;
            }
            if !e.session.is_empty() {
                sess.insert(e.session.as_str());
            }
            let b = (((e.ts_ms - start) as f64) / bucket) as usize;
            bars[b.min(nbars - 1)] += tok;
            for x in a {
                if x.name == "__agent_launch" {
                    continue;
                }
                let it = items.entry((x.name.clone(), x.kind)).or_insert_with(|| KeeperItem {
                    name: x.name.clone(),
                    kind: x.kind.into(),
                    tokens: 0.0,
                    cost: 0.0,
                    count: 0,
                    last_ms: 0,
                });
                it.tokens += tok * x.share;
                it.cost += cost * x.share;
                it.count += x.calls;
                if x.share > 0.0 || x.calls > 0 {
                    it.last_ms = it.last_ms.max(e.ts_ms);
                }
            }
        }
        // agents: count = number of distinct subagent transcripts in window
        let mut agent_files: HashMap<&str, HashSet<&str>> = HashMap::new();
        for (i, _, _, _) in &attrs {
            let e = &events[*i];
            if e.ts_ms >= start && !e.agent.is_empty() {
                agent_files.entry(e.agent.as_str()).or_default().insert(e.source.as_str());
            }
        }
        for ((name, kind), it) in items.iter_mut() {
            if *kind == "agent" {
                it.count = agent_files.get(name.as_str()).map(|s| s.len() as u64).unwrap_or(0);
            }
        }
        let mut v: Vec<KeeperItem> = items.into_values().filter(|x| x.tokens > 0.0 || x.count > 0).collect();
        v.sort_by(|a, b| b.tokens.partial_cmp(&a.tokens).unwrap_or(std::cmp::Ordering::Equal).then(b.count.cmp(&a.count)));
        if key == "month" {
            for it in &v {
                if matches!(it.kind.as_str(), "skill" | "agent" | "command") {
                    runs_30d.insert(it.name.clone(), it.count.max(if it.tokens > 0.0 { 1 } else { 0 }));
                }
            }
        }
        windows.push(KeeperWindow {
            key: key.into(),
            total_tokens: tt,
            total_cost: tc,
            requests: req,
            sessions: sess.len() as u64,
            bar_labels: bar_labels(key, start, span, nbars),
            bars,
            items: v,
        });
    }

    // ── hot sessions (activity in the last 5h) ──
    let start5 = now_ms - 5 * 3_600_000;
    struct S {
        tok: f64,
        cost: f64,
        last: i64,
        ctx_ts: i64,
        ctx: f64,
    }
    let mut by_s: HashMap<&str, S> = HashMap::new();
    for (i, tok, cost, _) in &attrs {
        let e = &events[*i];
        if e.ts_ms < start5 || e.session.is_empty() {
            continue;
        }
        let s = by_s.entry(e.session.as_str()).or_insert(S { tok: 0.0, cost: 0.0, last: 0, ctx_ts: 0, ctx: 0.0 });
        s.tok += tok;
        s.cost += cost;
        s.last = s.last.max(e.ts_ms);
        if e.agent.is_empty() && !e.model.is_empty() && e.ts_ms >= s.ctx_ts {
            s.ctx_ts = e.ts_ms;
            s.ctx = e.in_tok + e.cc + e.cr;
        }
    }
    let mut hot: Vec<HotSession> = by_s
        .into_iter()
        .map(|(sid, s)| {
            let info = sessions.get(sid).cloned().unwrap_or_default();
            let live = now_ms - s.last <= ACTIVE_MS;
            let mut reasons = Vec::new();
            if s.ctx >= HOT_CONTEXT {
                reasons.push(format!("context {}", fmt_k(s.ctx)));
            }
            if s.tok >= HOT_5H {
                reasons.push(format!("{} in 5h", fmt_k(s.tok)));
            }
            let project = info
                .cwd
                .rsplit(['/', '\\'])
                .find(|p| !p.is_empty())
                .unwrap_or("")
                .to_string();
            HotSession {
                session: sid.to_string(),
                project,
                cwd: info.cwd,
                title: info.title,
                context: s.ctx,
                tokens_5h: s.tok,
                cost_5h: s.cost,
                last_ms: s.last,
                hot: live && !reasons.is_empty(),
                reason: reasons.join(" · "),
            }
        })
        .collect();
    hot.sort_by(|a, b| b.hot.cmp(&a.hot).then(b.last_ms.cmp(&a.last_ms)));
    hot.truncate(12);
    let alert = hot.iter().any(|h| h.hot);

    Keeper { windows, sessions: hot, alert, last_used, runs_30d }
}

fn fmt_k(t: f64) -> String {
    if t >= 1e6 {
        format!("{:.1}M", t / 1e6)
    } else {
        format!("{:.0}K", t / 1e3)
    }
}

/// 5 evenly spaced tick labels for the histogram axis.
fn bar_labels(key: &str, start: i64, span: i64, _n: usize) -> Vec<String> {
    (0..5)
        .map(|k| {
            let ms = start + span * k / 4;
            let dt: DateTime<Local> = Local.timestamp_millis_opt(ms).single().unwrap_or_else(Local::now);
            match key {
                "5h" | "24h" => dt.format("%H:%M").to_string(),
                _ => dt.format("%b %-d").to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: i64, turn: &str, tools: &[&str], skills: &[&str], agent: &str, model: &str, tok: f64) -> RawEvent {
        RawEvent {
            ts_ms: ts,
            session: "s1".into(),
            model: model.into(),
            in_tok: tok,
            cc: 0.0,
            cr: 0.0,
            out_tok: 0.0,
            mcp: vec![],
            skills: skills.iter().map(|s| s.to_string()).collect(),
            id: format!("{ts}"),
            source: "f".into(),
            tools: tools.iter().map(|s| s.to_string()).collect(),
            agent: agent.into(),
            turn: turn.into(),
        }
    }

    #[test]
    fn attributes_tools_skills_agents() {
        let now = Local::now().timestamp_millis();
        let m = "claude-opus-4-1";
        let events = vec![
            ev(now - 5000, "t1", &["Edit", "Bash"], &[], "", m, 1000.0),
            ev(now - 4000, "t1", &["Skill"], &["plug:better-colors"], "", m, 400.0),
            ev(now - 3000, "t1", &["mcp__figma__get"], &[], "", m, 600.0),
            ev(now - 2000, "t2", &["Read"], &[], "", m, 100.0),
            ev(now - 1000, "t9", &["Read"], &[], "Explore", m, 50.0),
        ];
        let k = build(&events, &Pricing::shared(), &HashMap::new(), &HashSet::new());
        let w = &k.windows[0];
        let get = |n: &str, kind: &str| w.items.iter().find(|i| i.name == n && i.kind == kind).map(|i| i.tokens).unwrap_or(-1.0);
        assert_eq!(get("Edit", "tool"), 500.0);
        assert_eq!(get("Bash", "tool"), 500.0);
        // skill gets its own message + the rest of turn t1, not the earlier one or t2
        assert_eq!(get("better-colors", "skill"), 1000.0);
        assert_eq!(get("figma", "mcp"), 600.0);
        assert_eq!(get("Read", "tool"), 150.0);
        assert_eq!(get("Explore", "agent"), 50.0);
        assert_eq!(w.total_tokens, 2150.0);
        let sk = w.items.iter().find(|i| i.name == "better-colors").unwrap();
        assert_eq!(sk.count, 1);
    }
}
