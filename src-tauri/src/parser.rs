// Parse ~/.claude/projects/**/*.jsonl, dedupe assistant messages by id,
// classify tool calls (user-installed MCP / Skill only), and aggregate
// into Day / Week / Month reports + a daily heatmap.
use crate::config::UserConfig;
use crate::model::*;
use crate::pricing::Pricing;
use crate::store::{RawEvent, Store};
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// Serializes dashboard builds so the background refresh thread and the command
// handler never touch the incremental cache files concurrently.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

// One assistant API response, with config + pricing applied (derived per request
// from a RawEvent, since user config / prices / time windows can all change).
struct Event {
    ts: DateTime<Local>,
    sessions: Vec<String>,
    model: String,
    reqs: f64, // LLM requests this event stands for (1 per message; n for a rollup day)
    input: f64,  // raw tokens, uncached new input only
    cache: f64,  // raw tokens, cache creation + read
    output: f64, // raw tokens
    cost: f64,   // USD (differentiated by token type), 0 if unknown model
    priced: bool, // whether a price was found for this model
    mcp: Vec<String>,   // user-installed server names called in this msg
    skills: Vec<String>, // user-installed skill names called in this msg
}

// Top-5 models keep the green/slate scheme; everything beyond is uniform gray.
const PALETTE: &[&str] = &["#1f9d63", "#34c27e", "#6ad0a0", "#a7e3c5", "#4b5a52"];
const OVERFLOW_GRAY: &str = "#79817b";

/// Strip a trailing "-YYYYMMDD" date suffix so dated releases merge into
/// their base model (e.g. "claude-haiku-4-5-20251001" → "claude-haiku-4-5").
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub(crate) fn normalize_model(name: &str) -> String {
    if let Some(idx) = name.rfind('-') {
        let suffix = &name[idx + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return name[..idx].to_string();
        }
    }
    name.to_string()
}

fn vendor_of(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.contains("claude") {
        "Anthropic"
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") {
        "OpenAI"
    } else if m.contains("gemini") {
        "Google"
    } else if m.contains("llama") {
        "Local"
    } else if m.contains("glm") {
        "Zhipu"
    } else if m.contains("deepseek") {
        "DeepSeek"
    } else {
        "Other"
    }
}

pub fn build_dashboard() -> Dashboard {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 1. Ingest incrementally (full scan only on first run; afterwards just the
    //    appended lines), prune events older than the heatmap window, and persist
    //    only when something actually changed — so an idle tick doesn't rewrite
    //    the entire events.json every 30s.
    let mut store = Store::load();
    let mut dirty = store.ingest();
    // Raw events: 62 days (Day/Week/Month + previous month, keeper's 30d).
    // Older ones are folded into per-day rollups that are kept forever.
    let cutoff = (Local::now() - Duration::days(62)).timestamp_millis();
    if store.prune_before(cutoff) {
        dirty = true;
    }
    if dirty {
        store.save();
    }

    // 2. Aggregate: apply current config + prices, slice by current time.
    let cfg = UserConfig::load();
    // Memoized price table (cheap clone); loaded/refreshed off-thread elsewhere
    // so neither parsing nor the network runs while we hold BUILD_LOCK.
    let pricing = Pricing::shared();
    let mut events: Vec<Event> = store
        .events
        .iter()
        .map(|r| compute_event(r, &cfg, &pricing))
        .collect();
    events.extend(store.rollup.days.iter().filter_map(|d| roll_event(d, &cfg, &pricing)));

    let now = Local::now();
    let today = now.date_naive();

    // Keeper view (per skill/agent/tool attribution) works on the raw events.
    let user_commands: HashSet<String> = crate::skills::inventory()
        .into_iter()
        .filter(|i| i.kind == "command")
        .map(|i| i.name)
        .collect();
    let keeper = crate::keeper::build(&store.events, &pricing, &store.sessions, &user_commands, None);
    let accounts = crate::accounts::accounts();
    let account_views: Vec<crate::keeper::AccountView> = if accounts.len() > 1 {
        accounts
            .into_iter()
            .map(|a| crate::keeper::AccountView {
                keeper: crate::keeper::build(&store.events, &pricing, &store.sessions, &user_commands, Some(&a.dir)),
                account: a,
            })
            .collect()
    } else {
        accounts
            .into_iter()
            .map(|a| crate::keeper::AccountView { keeper: crate::keeper::Keeper::default(), account: a })
            .collect()
    };

    let mut day = report_day(&events, now);
    let mut week = report_week(&events, now);
    let mut month = report_month(&events, now);
    let mut all = report_all(&events, now);
    let heatmap = build_heatmap(&events, today);

    // "servers"/"skills" = how many the user has *installed* (global, constant
    // across periods), not how many were called in the window.
    let installed_servers = cfg.mcp_servers.len() as u64;
    let installed_skills = cfg.skills.len() as u64;
    for r in [&mut day, &mut week, &mut month, &mut all] {
        r.metrics.servers = installed_servers;
        r.metrics.skills = installed_skills;
    }

    // today's displayed tokens (M) for the tray
    let today_tokens: f64 = events
        .iter()
        .filter(|e| e.ts.date_naive() == today)
        .map(|e| (e.input + e.cache + e.output) / 1e6)
        .sum();

    Dashboard {
        day,
        week,
        month,
        all,
        heatmap,
        today_tokens,
        generated_at: now.to_rfc3339(),
        keeper,
        accounts: account_views,
    }
}

/// Derive a computed Event from a stored RawEvent, applying the *current* user
/// config (MCP/Skill whitelist) and prices. This is why these aren't baked into
/// the store: installing an MCP or a price refresh applies retroactively.
fn compute_event(r: &RawEvent, cfg: &UserConfig, pricing: &Pricing) -> Event {
    let ts = DateTime::from_timestamp_millis(r.ts_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    let model = normalize_model(&r.model);
    // price lookup uses the raw (possibly dated) id, then the normalized one
    let cost_opt = pricing
        .cost(&r.model, r.in_tok, r.out_tok, r.cc, r.cr)
        .or_else(|| pricing.cost(&model, r.in_tok, r.out_tok, r.cc, r.cr));
    let mcp = r
        .mcp
        .iter()
        .filter(|s| cfg.is_user_mcp(s))
        .cloned()
        .collect();
    let skills = r
        .skills
        .iter()
        .filter(|s| cfg.is_user_skill(s))
        .map(|s| s.rsplit(':').next().unwrap_or(s).to_string())
        .collect();
    Event {
        ts,
        sessions: if r.session.is_empty() { vec![] } else { vec![r.session.clone()] },
        reqs: if model.is_empty() { 0.0 } else { 1.0 },
        model,
        input: r.in_tok,
        cache: r.cc + r.cr,
        output: r.out_tok,
        cost: cost_opt.unwrap_or(0.0),
        priced: cost_opt.is_some(),
        mcp,
        skills,
    }
}

/// A rolled-up day × model as one Event (timestamped at local noon).
fn roll_event(d: &crate::store::DayRoll, cfg: &UserConfig, pricing: &Pricing) -> Option<Event> {
    use chrono::{NaiveDate, TimeZone};
    let date = NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").ok()?;
    let ts = Local.from_local_datetime(&date.and_hms_opt(12, 0, 0)?).single()?;
    let model = normalize_model(&d.model);
    let cost_opt = if d.model.is_empty() {
        None
    } else {
        pricing
            .cost(&d.model, d.in_tok, d.out_tok, d.cc, d.cr)
            .or_else(|| pricing.cost(&model, d.in_tok, d.out_tok, d.cc, d.cr))
    };
    let expand = |m: &std::collections::HashMap<String, u64>, keep: &dyn Fn(&str) -> bool, short: bool| -> Vec<String> {
        let mut v = Vec::new();
        for (k, n) in m {
            if keep(k) {
                let name = if short { k.rsplit(':').next().unwrap_or(k).to_string() } else { k.clone() };
                for _ in 0..*n {
                    v.push(name.clone());
                }
            }
        }
        v
    };
    Some(Event {
        ts,
        sessions: d.sessions.clone(),
        reqs: d.requests as f64,
        model,
        input: d.in_tok,
        cache: d.cc + d.cr,
        output: d.out_tok,
        cost: cost_opt.unwrap_or(0.0),
        priced: cost_opt.is_some(),
        mcp: expand(&d.mcp, &|s| cfg.is_user_mcp(s), false),
        skills: expand(&d.skills, &|s| cfg.is_user_skill(s), true),
    })
}

// ── aggregation helpers ────────────────────────────────────────────
#[derive(Default)]
struct Agg {
    input: f64,
    cache: f64,
    output: f64,
    cost: f64,
    requests: u64,
    sessions: HashSet<String>,
    mcp_calls: u64,
    skill_calls: u64,
    model_tok: HashMap<String, f64>,
    model_cost: HashMap<String, f64>,
    model_priced: HashMap<String, bool>,
    mcp_counts: HashMap<String, u64>,
    skill_counts: HashMap<String, u64>,
}

impl Agg {
    fn add(&mut self, e: &Event) {
        self.input += e.input;
        self.cache += e.cache;
        self.output += e.output;
        self.cost += e.cost;
        for s in &e.sessions {
            self.sessions.insert(s.clone());
        }
        // Slash-command skill events carry no model (empty) — they're not LLM
        // requests, so they must not inflate request counts or the model split.
        if !e.model.is_empty() {
            self.requests += e.reqs as u64;
            // model totals keep all token types so shares sum to Total tokens
            *self.model_tok.entry(e.model.clone()).or_default() += e.input + e.cache + e.output;
            *self.model_cost.entry(e.model.clone()).or_default() += e.cost;
            // a model is "priced" if any of its messages had a known price
            *self.model_priced.entry(e.model.clone()).or_default() |= e.priced;
        }
        for s in &e.mcp {
            self.mcp_calls += 1;
            *self.mcp_counts.entry(s.clone()).or_default() += 1;
        }
        for s in &e.skills {
            self.skill_calls += 1;
            *self.skill_counts.entry(s.clone()).or_default() += 1;
        }
    }

    fn models(&self) -> Vec<ModelStat> {
        let mut v: Vec<(String, f64, f64)> = self
            .model_tok
            .iter()
            .map(|(k, t)| (k.clone(), *t, *self.model_cost.get(k).unwrap_or(&0.0)))
            .collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .enumerate()
            .map(|(i, (name, tok, cost))| {
                let priced = *self.model_priced.get(&name).unwrap_or(&false);
                ModelStat {
                    vendor: vendor_of(&name).to_string(),
                    tokens: (tok / 1e6 * 100.0).round() / 100.0,
                    cost: (cost * 100.0).round() / 100.0,
                    color: if i < PALETTE.len() { PALETTE[i] } else { OVERFLOW_GRAY }.to_string(),
                    priced,
                    name,
                }
            })
            .collect()
    }

    fn named(counts: &HashMap<String, u64>) -> Vec<NamedCount> {
        let mut v: Vec<NamedCount> = counts
            .iter()
            .map(|(k, c)| NamedCount {
                name: k.clone(),
                count: *c,
            })
            .collect();
        v.sort_by(|a, b| b.count.cmp(&a.count));
        v
    }

    fn metrics(&self, delta_tokens: f64, delta_cost: f64) -> Metrics {
        Metrics {
            total_tokens: ((self.input + self.cache + self.output) / 1e6 * 100.0).round() / 100.0,
            input_tokens: (self.input / 1e6 * 100.0).round() / 100.0,
            cache_tokens: (self.cache / 1e6 * 100.0).round() / 100.0,
            output_tokens: (self.output / 1e6 * 100.0).round() / 100.0,
            cost: (self.cost * 100.0).round() / 100.0,
            mcp_calls: self.mcp_calls,
            skill_calls: self.skill_calls,
            requests: self.requests,
            sessions: self.sessions.len() as u64,
            delta_tokens,
            delta_cost,
            servers: self.mcp_counts.len() as u64,
            skills: self.skill_counts.len() as u64,
        }
    }
}

/// Percentage change of `cur` vs `prev`, e.g. +20.0 for a 20% increase,
/// rounded to 2 decimals. Returns 0 when there's no baseline to compare.
fn pct_delta(cur: f64, prev: f64) -> f64 {
    if prev <= 0.0 {
        return 0.0;
    }
    ((cur - prev) / prev * 10000.0).round() / 100.0
}

// ── Day report: today, 24 hourly buckets ───────────────────────────
fn report_day(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    let today = now.date_naive();
    let yesterday = today - Duration::days(1);
    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); 24]; // (input, cache, output) M
    let mut req_b = vec![0.0f64; 24];
    let mut cost_b = vec![0.0f64; 24];

    for e in events {
        let d = e.ts.date_naive();
        if d == today {
            agg.add(e);
            let h = e.ts.hour() as usize;
            buckets[h].0 += e.input / 1e6;
            buckets[h].1 += e.cache / 1e6;
            buckets[h].2 += e.output / 1e6;
            // Match Agg::add exactly: only the request COUNT excludes model-less
            // (slash-command) events; total cost accumulates unconditionally
            // (those events carry cost 0, so this is identical today).
            if !e.model.is_empty() {
                req_b[h] += e.reqs;
            }
            cost_b[h] += e.cost;
        } else if d == yesterday {
            prev.add(e);
        }
    }

    let series = (0..24)
        .map(|h| SeriesPoint {
            // axis ticks every 4h, skipping the 00/24 endpoints
            label: if h % 4 == 0 && h != 0 {
                format!("{:02}", h)
            } else {
                String::new()
            },
            full: format!("{:02}:00", h),
            input: buckets[h].0,
            cache: buckets[h].1,
            output: buckets[h].2,
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── Week report: current calendar week (Mon-Sun) vs previous week ────
fn report_week(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    let today = now.date_naive();
    // Monday of the current week (Mon=0 … Sun=6).
    let start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let next_start = start + Duration::days(7);
    let prev_start = start - Duration::days(7);

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); 7];
    let mut req_b = vec![0.0f64; 7];
    let mut cost_b = vec![0.0f64; 7];

    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d < next_start {
            agg.add(e);
            let idx = (d - start).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += e.reqs;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_start && d < start {
            prev.add(e);
        }
    }

    let weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let series = (0..7usize)
        .map(|i| {
            let date = start + Duration::days(i as i64);
            let wd = weekday[i];
            SeriesPoint {
                label: wd.to_string(),
                full: format!("{} {} {}", wd, MONTHS[(date.month() - 1) as usize], date.day()),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── Month report: current calendar month vs previous calendar month ──
fn report_month(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    use chrono::NaiveDate;
    let today = now.date_naive();
    let (y, m) = (today.year(), today.month());
    let cur_first = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let next_first = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1).unwrap()
    };
    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    let prev_first = NaiveDate::from_ymd_opt(py, pm, 1).unwrap();
    let days_in_month = (next_first - cur_first).num_days() as usize;

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); days_in_month];
    let mut req_b = vec![0.0f64; days_in_month];
    let mut cost_b = vec![0.0f64; days_in_month];

    for e in events {
        let d = e.ts.date_naive();
        if d >= cur_first && d < next_first {
            agg.add(e);
            let idx = (d - cur_first).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += e.reqs;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_first && d < cur_first {
            prev.add(e);
        }
    }

    let series = (0..days_in_month)
        .map(|i| {
            let dn = (i + 1) as u32;
            let label = if i == 0 || dn % 5 == 0 {
                dn.to_string()
            } else {
                String::new()
            };
            SeriesPoint {
                label,
                full: format!("{} {}", MONTHS[(m - 1) as usize], dn),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── All time: everything stored (raw + rollups) ──────────────────
// Daily bars while history is short, weekly up to ~6 months, monthly after.
fn report_all(events: &[Event], now: DateTime<Local>) -> PeriodReport {
    use chrono::NaiveDate;
    let today = now.date_naive();
    let first = events.iter().map(|e| e.ts.date_naive()).min().unwrap_or(today);
    let span = (today - first).num_days().max(0) + 1;
    // bucket start dates
    let mut starts: Vec<NaiveDate> = Vec::new();
    let mode = if span <= 62 { 0 } else if span <= 190 { 1 } else { 2 };
    match mode {
        0 => {
            let mut d = first;
            while d <= today {
                starts.push(d);
                d += Duration::days(1);
            }
        }
        1 => {
            let mut d = first - Duration::days(first.weekday().num_days_from_monday() as i64);
            while d <= today {
                starts.push(d);
                d += Duration::days(7);
            }
        }
        _ => {
            let (mut y, mut m) = (first.year(), first.month());
            loop {
                let d = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
                if d > today {
                    break;
                }
                starts.push(d);
                if m == 12 {
                    y += 1;
                    m = 1;
                } else {
                    m += 1;
                }
            }
        }
    }
    let n = starts.len().max(1);
    let mut agg = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); n];
    let mut req_b = vec![0.0f64; n];
    let mut cost_b = vec![0.0f64; n];
    for e in events {
        let d = e.ts.date_naive();
        if d > today {
            continue;
        }
        agg.add(e);
        let idx = match starts.binary_search(&d) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
        .min(n - 1);
        buckets[idx].0 += e.input / 1e6;
        buckets[idx].1 += e.cache / 1e6;
        buckets[idx].2 += e.output / 1e6;
        req_b[idx] += e.reqs;
        cost_b[idx] += e.cost;
    }
    let every = (n / 6).max(1);
    let series = (0..n)
        .map(|i| {
            let d = starts.get(i).copied().unwrap_or(today);
            let mon = MONTHS[(d.month() - 1) as usize];
            let (label, full) = match mode {
                0 => (format!("{}", d.day()), format!("{} {}", mon, d.day())),
                1 => (format!("{} {}", mon, d.day()), format!("Week of {} {}", mon, d.day())),
                _ => (mon.to_string(), format!("{} {}", mon, d.year())),
            };
            SeriesPoint {
                label: if i % every == 0 { label } else { String::new() },
                full,
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();
    PeriodReport {
        metrics: agg.metrics(0.0, 0.0),
        series,
        models: agg.models(),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
    }
}

// ── Heatmap: last ~26 weeks daily totals ────────────────────────────
fn build_heatmap(events: &[Event], today: chrono::NaiveDate) -> Vec<HeatDay> {
    let start = today - Duration::days(25 * 7 + today.weekday().num_days_from_sunday() as i64);
    let mut by_day: HashMap<chrono::NaiveDate, f64> = HashMap::new();
    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d <= today {
            *by_day.entry(d).or_default() += (e.input + e.cache + e.output) / 1e6;
        }
    }
    let mut days = Vec::new();
    let mut d = start;
    let mut maxv = 0.0f64;
    while d <= today {
        let t = *by_day.get(&d).unwrap_or(&0.0);
        maxv = maxv.max(t);
        days.push((d, t));
        d += Duration::days(1);
    }
    days.into_iter()
        .map(|(date, tokens)| {
            let f = if maxv > 0.0 { tokens / maxv } else { 0.0 };
            let level = if tokens == 0.0 {
                0
            } else if f < 0.25 {
                1
            } else if f < 0.5 {
                2
            } else if f < 0.75 {
                3
            } else {
                4
            };
            HeatDay {
                date: date.format("%Y-%m-%d").to_string(),
                tokens: (tokens * 100.0).round() / 100.0,
                level,
            }
        })
        .collect()
}
