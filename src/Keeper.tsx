import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AccountView, Dashboard, HotSession, InvItem, ItemKind, Keeper, KeeperItem, KeeperWindow, fetchInventory, fmtMoney, inTauri } from "./data";

// ── theme ─────────────────────────────────────────────────────────
interface KT {
  card: string; raised: string; sheet: string; line: string; text: string; dim: string; faint: string;
  green: string; greenDim: string; track: string; amber: string; amberBg: string; red: string;
  seg: string; segOn: string; hover: string; ui: string; mono: string; input: string;
}
const KTH: Record<"dark" | "light", KT> = {
  dark: {
    card: "#2a2b2e", raised: "#333438", sheet: "#36373b", line: "rgba(255,255,255,0.08)",
    text: "rgba(255,255,255,0.92)", dim: "rgba(255,255,255,0.55)", faint: "rgba(255,255,255,0.32)",
    green: "#5fd47a", greenDim: "#3a9a52", track: "rgba(255,255,255,0.10)", amber: "#ffcc4d", amberBg: "rgba(255,204,77,0.14)", red: "#ff7a6b",
    seg: "rgba(255,255,255,0.07)", segOn: "rgba(255,255,255,0.18)", hover: "rgba(255,255,255,0.05)",
    ui: "-apple-system, BlinkMacSystemFont, 'SF Pro Text', 'Segoe UI', system-ui, sans-serif",
    mono: "ui-monospace, 'SF Mono', Menlo, Consolas, monospace", input: "rgba(0,0,0,0.25)",
  },
  light: {
    card: "#f6f6f7", raised: "#ffffff", sheet: "#ffffff", line: "rgba(0,0,0,0.08)",
    text: "rgba(0,0,0,0.88)", dim: "rgba(0,0,0,0.52)", faint: "rgba(0,0,0,0.32)",
    green: "#1f9d45", greenDim: "#7fcf95", track: "rgba(0,0,0,0.08)", amber: "#b77800", amberBg: "rgba(255,180,0,0.16)", red: "#d93a2b",
    seg: "rgba(0,0,0,0.05)", segOn: "#ffffff", hover: "rgba(0,0,0,0.04)",
    ui: "-apple-system, BlinkMacSystemFont, 'SF Pro Text', 'Segoe UI', system-ui, sans-serif",
    mono: "ui-monospace, 'SF Mono', Menlo, Consolas, monospace", input: "rgba(0,0,0,0.04)",
  },
};

// ── formatting ────────────────────────────────────────────────────
export const fmtTok = (t: number) => {
  if (t >= 1e9) return (t / 1e9).toFixed(t >= 1e10 ? 0 : 1) + " B";
  if (t >= 1e6) return (t / 1e6).toFixed(t >= 1e8 ? 0 : 1) + " M";
  if (t >= 1e3) return Math.round(t / 1e3) + " K";
  return Math.round(t).toString();
};
const ago = (ms: number) => {
  if (!ms) return "never";
  const s = (Date.now() - ms) / 1000;
  if (s < 60) return "just now";
  if (s < 3600) return Math.floor(s / 60) + "m ago";
  if (s < 86400) return Math.floor(s / 3600) + "h ago";
  return Math.floor(s / 86400) + "d ago";
};
const KIND_LABEL: Record<string, string> = { skill: "skill", agent: "agent", tool: "tool", mcp: "MCP", command: "command" };
const PERIODS: { key: KeeperWindow["key"]; label: string }[] = [
  { key: "5h", label: "5h" }, { key: "24h", label: "24h" }, { key: "week", label: "Week" }, { key: "month", label: "Month" },
];
type Filter = "all" | ItemKind;
const FILTERS: { key: Filter; label: string }[] = [
  { key: "all", label: "All" }, { key: "skill", label: "Skills" }, { key: "agent", label: "Agents" },
  { key: "tool", label: "Tools" }, { key: "mcp", label: "MCP" }, { key: "command", label: "Commands" },
];

function lsGet<T extends string>(k: string, def: T, ok: readonly string[]): T {
  try { const v = localStorage.getItem(k); if (v && ok.includes(v)) return v as T; } catch {}
  return def;
}
function lsSet(k: string, v: string) { try { localStorage.setItem(k, v); } catch {} }

async function copy(text: string) {
  try { await navigator.clipboard.writeText(text); return true; } catch {
    const ta = document.createElement("textarea"); ta.value = text; document.body.appendChild(ta); ta.select();
    const ok = document.execCommand("copy"); ta.remove(); return ok;
  }
}

// ── small atoms ───────────────────────────────────────────────────
function Seg<T extends string>({ items, value, onChange, k }: { items: { key: T; label: string }[]; value: T; onChange: (v: T) => void; k: KT }) {
  return (
    <div style={{ display: "flex", background: k.seg, borderRadius: 7, padding: 2, gap: 1 }}>
      {items.map((it) => (
        <button key={it.key} onClick={() => onChange(it.key)} style={{
          flex: 1, border: 0, cursor: "pointer", borderRadius: 5, padding: "3px 9px",
          font: `500 11.5px ${k.ui}`, color: value === it.key ? k.text : k.dim,
          background: value === it.key ? k.segOn : "transparent",
          boxShadow: value === it.key ? "0 1px 2px rgba(0,0,0,0.25)" : "none",
        }}>{it.label}</button>
      ))}
    </div>
  );
}

function Bolt({ color, size = 14 }: { color: string; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden>
      <path d="M13.2 2 4.5 13.4h6.1L9.8 22l8.7-11.5h-6.1L13.2 2z" fill={color} />
    </svg>
  );
}

function Btn({ children, onClick, k, tone = "plain", disabled, title }: { children: React.ReactNode; onClick?: () => void; k: KT; tone?: "plain" | "primary" | "warn"; disabled?: boolean; title?: string }) {
  const bg = tone === "primary" ? k.green : tone === "warn" ? k.amberBg : k.seg;
  const fg = tone === "primary" ? "#08250f" : tone === "warn" ? k.amber : k.text;
  return (
    <button title={title} disabled={disabled} onClick={onClick} style={{
      border: 0, borderRadius: 6, padding: "4px 10px", cursor: disabled ? "default" : "pointer",
      font: `500 11.5px ${k.ui}`, background: bg, color: fg, opacity: disabled ? 0.45 : 1, whiteSpace: "nowrap",
    }}>{children}</button>
  );
}

// ── histogram ─────────────────────────────────────────────────────
function Histogram({ w, k }: { w: KeeperWindow; k: KT }) {
  const max = Math.max(...w.bars, 1);
  const [hover, setHover] = useState<number | null>(null);
  return (
    <div style={{ padding: "6px 14px 0" }}>
      <div style={{ display: "flex", alignItems: "flex-end", gap: 2, height: 54, position: "relative" }}
        onMouseLeave={() => setHover(null)}>
        {w.bars.map((b, i) => (
          <div key={i} onMouseEnter={() => setHover(i)} style={{ flex: 1, height: "100%", display: "flex", alignItems: "flex-end" }}>
            <div style={{
              width: "100%", height: b > 0 ? Math.max(2, (b / max) * 54) : 1, borderRadius: 2,
              background: b > 0 ? (hover === i ? k.green : k.greenDim) : k.track, transition: "height .25s",
            }} />
          </div>
        ))}
        {hover !== null && (
          <div style={{ position: "absolute", top: -4, right: 0, font: `500 10.5px ${k.mono}`, color: k.text,
            background: k.raised, padding: "1px 6px", borderRadius: 4, border: `1px solid ${k.line}` }}>
            {fmtTok(w.bars[hover])}
          </div>
        )}
      </div>
      <div style={{ display: "flex", justifyContent: "space-between", marginTop: 4 }}>
        {w.barLabels.map((l, i) => (
          <span key={i} style={{ font: `400 9.5px ${k.mono}`, color: k.faint }}>{l}</span>
        ))}
      </div>
    </div>
  );
}

// ── usage tab ─────────────────────────────────────────────────────
function UsageRow({ it, max, total, sortCost, k, onOpen, onCopy }: { it: KeeperItem; max: number; total: number; sortCost: boolean; k: KT; onOpen: () => void; onCopy: () => void }) {
  const v = sortCost ? it.cost : it.tokens;
  const share = total > 0 ? (it.tokens / total) * 100 : 0;
  const clickable = it.kind === "skill" || it.kind === "agent" || it.kind === "command";
  const tip = `${it.name} · ${KIND_LABEL[it.kind]}\n${fmtTok(it.tokens)} tokens (${share.toFixed(1)}% of window) · ${fmtMoney(it.cost)}` +
    `\n${it.count} ${it.kind === "agent" ? "runs" : "calls"} · last ${ago(it.lastMs)}` +
    (clickable ? "\nClick for details · right-click copies /" + it.name : "\nRight-click copies the name");
  return (
    <div title={tip} onClick={clickable ? onOpen : undefined}
      onContextMenu={(e) => { e.preventDefault(); onCopy(); }}
      className="kp-row"
      style={{ display: "flex", alignItems: "center", gap: 8, padding: "5px 14px", cursor: clickable ? "pointer" : "default" }}>
      <div style={{ flex: 1, minWidth: 0, display: "flex", alignItems: "baseline", gap: 5, overflow: "hidden" }}>
        <span style={{ font: `500 12.5px ${k.ui}`, color: k.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{it.name}</span>
        <span style={{ font: `italic 400 11px ${k.ui}`, color: k.faint, flexShrink: 0 }}>{KIND_LABEL[it.kind]}</span>
        {it.count > 0 && <span style={{ font: `400 10px ${k.mono}`, color: k.faint, flexShrink: 0 }}>×{it.count}</span>}
      </div>
      <div style={{ width: 54, height: 4, background: k.track, borderRadius: 2, flexShrink: 0 }}>
        <div style={{ width: `${Math.max(3, (v / (max || 1)) * 100)}%`, height: "100%", borderRadius: 2, background: k.green }} />
      </div>
      <span style={{ width: 58, textAlign: "right", font: `500 12px ${k.mono}`, color: k.green, flexShrink: 0 }}>
        {sortCost ? fmtMoney(it.cost) : fmtTok(it.tokens)}
      </span>
    </div>
  );
}

function UsageTab({ keeper, k, onOpenItem, toast }: { keeper: Keeper; k: KT; onOpenItem: (name: string, kind: string) => void; toast: (m: string) => void }) {
  const [period, setPeriod] = useState<KeeperWindow["key"]>(() => lsGet("kp-period", "24h", ["5h", "24h", "week", "month"]));
  const [filter, setFilter] = useState<Filter>(() => lsGet("kp-filter", "all", ["all", "skill", "agent", "tool", "mcp", "command"]));
  const [sortCost, setSortCost] = useState(() => lsGet<"usage" | "cost">("kp-sort", "usage", ["usage", "cost"]) === "cost");
  const w = keeper.windows.find((x) => x.key === period) ?? keeper.windows[0];
  const items = useMemo(() => {
    const v = w.items.filter((i) => filter === "all" || i.kind === filter);
    return [...v].sort((a, b) => (sortCost ? b.cost - a.cost : b.tokens - a.tokens));
  }, [w, filter, sortCost]);
  const max = Math.max(...items.map((i) => (sortCost ? i.cost : i.tokens)), 0);
  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const i of w.items) c[i.kind] = (c[i.kind] || 0) + 1;
    return c;
  }, [w]);

  return (
    <div style={{ display: "flex", flexDirection: "column", minHeight: 0, flex: 1 }}>
      <div style={{ display: "flex", gap: 6, padding: "10px 14px 0", alignItems: "center" }}>
        <div style={{ flex: 1 }}>
          <Seg k={k} items={PERIODS} value={period} onChange={(v) => { setPeriod(v); lsSet("kp-period", v); }} />
        </div>
        <Seg k={k} items={[{ key: "usage", label: "Tokens" }, { key: "cost", label: "$" }]} value={sortCost ? "cost" : "usage"}
          onChange={(v) => { setSortCost(v === "cost"); lsSet("kp-sort", v); }} />
      </div>
      <Histogram w={w} k={k} />
      <div style={{ display: "flex", gap: 10, padding: "8px 14px 6px", flexWrap: "wrap" }}>
        {FILTERS.filter((f) => f.key === "all" || counts[f.key]).map((f) => (
          <button key={f.key} onClick={() => { setFilter(f.key); lsSet("kp-filter", f.key); }} style={{
            border: 0, background: "none", padding: 0, cursor: "pointer",
            font: `${filter === f.key ? 600 : 400} 12px ${k.ui}`, color: filter === f.key ? k.text : k.dim,
          }}>{f.label}{f.key !== "all" && <span style={{ color: k.faint, fontWeight: 400 }}> {counts[f.key]}</span>}</button>
        ))}
      </div>
      <div className="om-scroll" style={{ flex: 1, overflowY: "auto", minHeight: 0, borderTop: `1px solid ${k.line}`, paddingTop: 4 }}>
        {items.length === 0 && (
          <div style={{ padding: "30px 14px", textAlign: "center", font: `400 12px ${k.ui}`, color: k.dim }}>
            Nothing ran in this window.
          </div>
        )}
        {items.map((it) => (
          <UsageRow key={it.kind + it.name} it={it} max={max} total={w.totalTokens} sortCost={sortCost} k={k}
            onOpen={() => onOpenItem(it.name, it.kind)}
            onCopy={async () => {
              const txt = it.kind === "skill" || it.kind === "command" ? "/" + it.name : it.name;
              if (await copy(txt)) toast(`${txt} copied`);
            }} />
        ))}
      </div>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", padding: "8px 14px", borderTop: `1px solid ${k.line}` }}>
        <span style={{ font: `500 12px ${k.ui}`, color: k.text }}>Total tokens</span>
        <span style={{ font: `400 11px ${k.mono}`, color: k.dim }}>
          {fmtMoney(w.totalCost)} · {w.sessions} sess · <span style={{ color: k.green, fontWeight: 600, fontSize: 12.5 }}>{fmtTok(w.totalTokens)}</span>
        </span>
      </div>
    </div>
  );
}

// ── skills tab ────────────────────────────────────────────────────
type SFilter = "all" | "active" | "paused" | "unused" | "issues";
function SkillsTab({ inv, keeper, k, onOpen, onToggle, busy, acctName }: { inv: InvItem[]; keeper: Keeper; k: KT; onOpen: (it: InvItem) => void; onToggle: (it: InvItem) => void; busy: string | null; acctName?: (dir: string) => string | undefined }) {
  const [q, setQ] = useState("");
  const [f, setF] = useState<SFilter>("all");
  const rows = inv.filter((it) => {
    if (q && !(it.name + " " + it.description + " " + it.plugin).toLowerCase().includes(q.toLowerCase())) return false;
    const runs = keeper.runs30d[it.name] || 0;
    if (f === "active") return it.enabled;
    if (f === "paused") return !it.enabled;
    if (f === "unused") return it.enabled && runs === 0;
    if (f === "issues") return it.warnings.length > 0;
    return true;
  });
  const groups: [string, InvItem[]][] = [
    ["Skills", rows.filter((r) => r.kind === "skill")],
    ["Agents", rows.filter((r) => r.kind === "agent")],
    ["Commands", rows.filter((r) => r.kind === "command")],
  ];
  const nUnused = inv.filter((it) => it.enabled && !(keeper.runs30d[it.name] > 0)).length;
  const nIssues = inv.filter((it) => it.warnings.length).length;
  return (
    <div style={{ display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
      <div style={{ padding: "10px 14px 6px", display: "flex", flexDirection: "column", gap: 8 }}>
        <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search skills, agents, commands"
          style={{ border: `1px solid ${k.line}`, background: k.input, color: k.text, borderRadius: 7, padding: "6px 9px", font: `400 12px ${k.ui}`, outline: "none" }} />
        <div style={{ display: "flex", gap: 10, flexWrap: "wrap" }}>
          {([["all", "All"], ["active", "Active"], ["paused", "Paused"], ["unused", `Unused 30d · ${nUnused}`], ["issues", `Issues · ${nIssues}`]] as [SFilter, string][]).map(([key, label]) => (
            <button key={key} onClick={() => setF(key)} style={{ border: 0, background: "none", padding: 0, cursor: "pointer",
              font: `${f === key ? 600 : 400} 12px ${k.ui}`, color: f === key ? k.text : k.dim }}>{label}</button>
          ))}
        </div>
      </div>
      <div className="om-scroll" style={{ flex: 1, overflowY: "auto", minHeight: 0, borderTop: `1px solid ${k.line}` }}>
        {inv.length === 0 && <div style={{ padding: 24, textAlign: "center", font: `400 12px ${k.ui}`, color: k.dim }}>No skills found in ~/.claude</div>}
        {groups.filter(([, v]) => v.length).map(([title, v]) => (
          <div key={title}>
            <div style={{ padding: "10px 14px 4px", font: `600 10.5px ${k.ui}`, letterSpacing: ".06em", textTransform: "uppercase", color: k.faint }}>{title} · {v.length}</div>
            {v.map((it) => {
              const runs = keeper.runs30d[it.name] || 0;
              const last = keeper.lastUsed[it.name] || 0;
              return (
                <div key={it.file} className="kp-row" onClick={() => onOpen(it)}
                  style={{ display: "flex", alignItems: "center", gap: 9, padding: "6px 14px", cursor: "pointer", opacity: it.enabled ? 1 : 0.55 }}>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ display: "flex", alignItems: "baseline", gap: 6 }}>
                      <span style={{ font: `500 12.5px ${k.ui}`, color: k.text, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{it.name}</span>
                      {acctName?.(it.account) && <span style={{ font: `500 10px ${k.ui}`, color: k.dim, border: `1px solid ${k.line}`, borderRadius: 4, padding: "0 4px" }}>{acctName(it.account)}</span>}
                      {it.scope === "plugin" && <span style={{ font: `400 10.5px ${k.ui}`, color: k.faint, whiteSpace: "nowrap" }}>{it.plugin}</span>}
                      {it.warnings.length > 0 && <span title={it.warnings.join("\n")} style={{ color: k.amber, font: `600 11px ${k.ui}` }}>⚠</span>}
                    </div>
                    <div style={{ font: `400 10.5px ${k.mono}`, color: runs ? k.dim : k.faint, marginTop: 1 }}>
                      {runs ? `${runs} runs / 30d · ${ago(last)}` : last ? `last ${ago(last)}` : "no runs in 30 days"} · {it.descLen} chars
                    </div>
                  </div>
                  {it.editable ? (
                    <Toggle on={it.enabled} k={k} disabled={busy === it.root}
                      onChange={() => onToggle(it)} title={it.enabled ? "Pause — Claude stops loading it" : "Resume"} />
                  ) : (
                    <span title="Plugin skills are managed by the plugin" style={{ font: `400 10px ${k.ui}`, color: k.faint }}>plugin</span>
                  )}
                </div>
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}

function Toggle({ on, onChange, k, disabled, title }: { on: boolean; onChange: () => void; k: KT; disabled?: boolean; title?: string }) {
  return (
    <button title={title} disabled={disabled} onClick={(e) => { e.stopPropagation(); onChange(); }} style={{
      width: 30, height: 18, borderRadius: 9, border: 0, padding: 2, cursor: "pointer", flexShrink: 0,
      background: on ? k.green : k.track, display: "flex", justifyContent: on ? "flex-end" : "flex-start", opacity: disabled ? 0.5 : 1,
      transition: "background .15s",
    }}>
      <span style={{ width: 14, height: 14, borderRadius: 7, background: "#fff", boxShadow: "0 1px 2px rgba(0,0,0,0.3)" }} />
    </button>
  );
}

// ── sessions tab ──────────────────────────────────────────────────
function SessionsTab({ sessions, k, toast, accounts }: { sessions: HotSession[]; k: KT; toast: (m: string) => void; accounts: AccountView[] }) {
  const acctOf = (dir: string) => accounts.find((a) => a.dir === dir);
  return (
    <div className="om-scroll" style={{ flex: 1, overflowY: "auto", minHeight: 0, paddingTop: 6 }}>
      <div style={{ padding: "4px 14px 8px", font: `400 11px ${k.ui}`, color: k.dim }}>
        Sessions active in the last 5 hours. Flagged when the live context passes 300K tokens or a session burns 150M in 5h. Click to copy a resume command.
      </div>
      {sessions.length === 0 && <div style={{ padding: 24, textAlign: "center", font: `400 12px ${k.ui}`, color: k.dim }}>No sessions in the last 5 hours.</div>}
      {sessions.map((s) => (
        <div key={s.session} className="kp-row"
          onClick={async () => {
            const acct = acctOf(s.configDir);
            const env = acct && !acct.isDefault ? `CLAUDE_CONFIG_DIR=${JSON.stringify(acct.dir)} ` : "";
            const cmd = (s.cwd ? `cd ${JSON.stringify(s.cwd)} && ` : "") + `${env}claude --resume ${s.session}`;
            if (await copy(cmd)) toast("Resume command copied");
          }}
          title={`${s.cwd}\n${s.session}`}
          style={{ padding: "8px 14px", cursor: "pointer", borderLeft: `3px solid ${s.hot ? k.amber : "transparent"}`, background: s.hot ? k.amberBg : undefined }}>
          <div style={{ display: "flex", justifyContent: "space-between", gap: 8 }}>
            <span style={{ font: `500 12.5px ${k.ui}`, color: k.text, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
              {s.title || s.project || s.session.slice(0, 8)}
            </span>
            <span style={{ font: `500 12px ${k.mono}`, color: s.hot ? k.amber : k.green, flexShrink: 0 }}>{fmtTok(s.context)} ctx</span>
          </div>
          <div style={{ display: "flex", justifyContent: "space-between", marginTop: 2, font: `400 10.5px ${k.mono}`, color: k.dim }}>
            <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
              {accounts.length > 1 && acctOf(s.configDir) && <span style={{ color: k.text }}>{acctOf(s.configDir)!.name} · </span>}
              {s.project} · {ago(s.lastMs)}
            </span>
            <span style={{ flexShrink: 0 }}>{fmtTok(s.tokens5h)} · {fmtMoney(s.cost5h)} / 5h</span>
          </div>
          {s.hot && <div style={{ marginTop: 3, font: `500 10.5px ${k.ui}`, color: k.amber }}>⚠ {s.reason}</div>}
        </div>
      ))}
    </div>
  );
}

// ── detail sheet ──────────────────────────────────────────────────
function Detail({ it, usage, keeper, k, onClose, onToggle, onSaved, toast }: {
  it: InvItem | null; usage: { name: string; kind: string }; keeper: Keeper; k: KT;
  onClose: () => void; onToggle: (it: InvItem) => void; onSaved: () => void; toast: (m: string) => void;
}) {
  const [desc, setDesc] = useState(it?.description ?? "");
  const [editing, setEditing] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => { setDesc(it?.description ?? ""); setEditing(false); setErr(null); }, [it?.file]);
  const per = keeper.windows.map((w) => ({ key: w.key, row: w.items.find((x) => x.name === usage.name && x.kind === usage.kind) }));
  const slash = usage.kind === "skill" || usage.kind === "command" ? "/" + usage.name : null;
  const save = async () => {
    if (!it) return;
    try {
      await invoke("set_item_description", { file: it.file, text: desc });
      setEditing(false); setErr(null); toast("Description saved · backup in ~/.claude/.tokenscope-backup"); onSaved();
    } catch (e) { setErr(String(e)); }
  };
  const open = (reveal: boolean) => it && invoke("open_item", { path: it.file, reveal }).catch((e) => setErr(String(e)));
  return (
    <div onClick={onClose} style={{ position: "absolute", inset: 0, background: "rgba(0,0,0,0.35)", zIndex: 30, display: "flex", alignItems: "flex-end" }}>
      <div onClick={(e) => e.stopPropagation()} className="om-scroll" style={{
        width: "100%", maxHeight: "86%", overflowY: "auto", background: k.sheet, borderTop: `1px solid ${k.line}`,
        borderRadius: "12px 12px 0 0", padding: "14px 14px 16px", boxShadow: "0 -8px 30px rgba(0,0,0,0.35)",
      }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ font: `600 15px ${k.ui}`, color: k.text, flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{usage.name}</span>
          <span style={{ font: `italic 400 11.5px ${k.ui}`, color: k.faint }}>{KIND_LABEL[usage.kind]}{it?.scope === "plugin" ? ` · ${it.plugin}` : ""}{it && !it.enabled ? " · paused" : ""}</span>
          <button onClick={onClose} style={{ border: 0, background: "none", color: k.dim, cursor: "pointer", fontSize: 16 }}>×</button>
        </div>

        <div style={{ display: "grid", gridTemplateColumns: "repeat(4, 1fr)", gap: 6, marginTop: 12 }}>
          {per.map(({ key, row }) => (
            <div key={key} style={{ background: k.seg, borderRadius: 7, padding: "6px 8px" }}>
              <div style={{ font: `400 10px ${k.ui}`, color: k.faint }}>{PERIODS.find((p) => p.key === key)?.label}</div>
              <div style={{ font: `600 12.5px ${k.mono}`, color: row ? k.green : k.faint }}>{row ? fmtTok(row.tokens) : "—"}</div>
              <div style={{ font: `400 10px ${k.mono}`, color: k.dim }}>{row ? `${fmtMoney(row.cost)} · ×${row.count}` : ""}</div>
            </div>
          ))}
        </div>
        <div style={{ marginTop: 6, font: `400 10.5px ${k.ui}`, color: k.faint }}>
          Last run {ago(keeper.lastUsed[usage.name] || 0)}. Tokens = everything Claude spent from the moment it fired to the end of that turn.
        </div>

        {it ? (
          <>
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginTop: 14 }}>
              <span style={{ font: `600 11.5px ${k.ui}`, color: k.text }}>Description <span style={{ color: it.descLen > 1024 ? k.red : k.faint, fontWeight: 400 }}>{desc.length} chars</span></span>
              {it.editable && !editing && <Btn k={k} onClick={() => setEditing(true)}>Edit</Btn>}
            </div>
            {editing ? (
              <>
                <textarea value={desc} onChange={(e) => setDesc(e.target.value)} rows={6} style={{
                  width: "100%", marginTop: 6, background: k.input, color: k.text, border: `1px solid ${k.line}`, borderRadius: 7,
                  padding: 8, font: `400 12px ${k.ui}`, resize: "vertical", outline: "none" }} />
                <div style={{ display: "flex", gap: 6, marginTop: 6, justifyContent: "flex-end" }}>
                  <Btn k={k} onClick={() => { setDesc(it.description); setEditing(false); }}>Cancel</Btn>
                  <Btn k={k} tone="primary" onClick={save}>Save</Btn>
                </div>
              </>
            ) : (
              <div style={{ marginTop: 6, font: `400 12px/1.45 ${k.ui}`, color: it.description ? k.text : k.faint, whiteSpace: "pre-wrap" }}>
                {it.description || "No description."}
              </div>
            )}
            {it.warnings.length > 0 && (
              <div style={{ marginTop: 10, padding: "6px 9px", borderRadius: 7, background: k.amberBg, font: `500 11px ${k.ui}`, color: k.amber }}>
                {it.warnings.map((w) => <div key={w}>⚠ {w}</div>)}
              </div>
            )}
            <div style={{ marginTop: 12, font: `400 10.5px ${k.mono}`, color: k.faint, wordBreak: "break-all" }}>{it.file}</div>
            <div style={{ display: "flex", gap: 6, marginTop: 10, flexWrap: "wrap" }}>
              <Btn k={k} onClick={() => open(false)}>Open file</Btn>
              <Btn k={k} onClick={() => open(true)}>Show in Finder</Btn>
              {slash && <Btn k={k} onClick={async () => { if (await copy(slash)) toast(`${slash} copied`); }}>Copy {slash}</Btn>}
              {it.editable && (
                <Btn k={k} tone={it.enabled ? "warn" : "primary"} onClick={() => onToggle(it)}>{it.enabled ? "Pause" : "Resume"}</Btn>
              )}
            </div>
          </>
        ) : (
          <div style={{ marginTop: 14, font: `400 11.5px ${k.ui}`, color: k.dim }}>
            Not found in ~/.claude — it's built in, comes from a project folder, or was removed.
            {slash && <div style={{ marginTop: 8 }}><Btn k={k} onClick={async () => { if (await copy(slash)) toast(`${slash} copied`); }}>Copy {slash}</Btn></div>}
          </div>
        )}
        {err && <div style={{ marginTop: 8, font: `500 11px ${k.ui}`, color: k.red }}>{err}</div>}
      </div>
    </div>
  );
}

// ── shell ─────────────────────────────────────────────────────────
export type KTab = "usage" | "skills" | "sessions" | "charts";
export function KeeperShell({ dash, dark, tab, setTab, openGen, children }: {
  dash: Dashboard; dark: boolean; tab: KTab; setTab: (t: KTab) => void; openGen: number; children?: React.ReactNode;
}) {
  const k = KTH[dark ? "dark" : "light"];
  const accounts = dash.accounts ?? [];
  const multi = accounts.length > 1;
  const [acct, setAcct] = useState<string>(() => { try { return localStorage.getItem("kp-acct") || "all"; } catch { return "all"; } });
  const cur = multi && acct !== "all" ? accounts.find((a) => a.dir === acct) : undefined;
  const keeper = cur ? cur.keeper : dash.keeper;
  const pickAcct = (v: string) => { setAcct(v); lsSet("kp-acct", v); };
  const [pinned, setPinned] = useState<boolean>(() => { try { return localStorage.getItem("kp-pinned") === "1"; } catch { return false; } });
  useEffect(() => { if (inTauri()) invoke("set_pinned", { pinned }).catch(() => {}); }, [pinned]);
  const togglePin = () => setPinned((p) => { lsSet("kp-pinned", p ? "0" : "1"); return !p; });
  const [invAll, setInv] = useState<InvItem[]>([]);
  // In "All" mode with several accounts, skills are listed per account; the
  // selector narrows to one. Single account → everything.
  const inv = cur ? invAll.filter((i) => i.account === cur.dir) : invAll;
  const [sel, setSel] = useState<{ name: string; kind: string } | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const timer = useRef<number | null>(null);
  const toast = (m: string) => {
    if (timer.current) window.clearTimeout(timer.current);
    setMsg(m); timer.current = window.setTimeout(() => setMsg(null), 1800);
  };
  const reload = () => fetchInventory().then(setInv).catch(() => {});
  useEffect(() => { reload(); }, [openGen]);

  const selInv = sel ? inv.find((i) => i.name === sel.name && (i.kind === sel.kind || (sel.kind === "command" && i.kind === "skill") || (sel.kind === "skill" && i.kind === "command"))) ?? null : null;
  const toggle = async (it: InvItem) => {
    if (!inTauri()) return;
    setBusy(it.root);
    try {
      await invoke("toggle_item", { root: it.root, kind: it.kind, enable: !it.enabled });
      toast(it.enabled ? `${it.name} paused — new sessions won't load it` : `${it.name} resumed`);
      await reload();
    } catch (e) { toast(String(e)); }
    setBusy(null);
  };

  const hotCount = keeper?.sessions.filter((s) => s.hot).length ?? 0;
  const tabs: { key: KTab; label: string }[] = [
    { key: "usage", label: "Usage" }, { key: "skills", label: "Skills" },
    { key: "sessions", label: hotCount ? `Sessions·${hotCount}` : "Sessions" }, { key: "charts", label: "Charts" },
  ];

  return (
    <div style={{ height: "100vh", display: "flex", flexDirection: "column", background: k.card, color: k.text,
      borderRadius: 12, border: `1px solid ${k.line}`, overflow: "hidden", position: "relative", fontFamily: k.ui }}>
      <style>{`.kp-row:hover{background:${k.hover}}`}</style>
      <div
        onMouseDown={(e) => {
          if (!pinned || e.button !== 0 || (e.target as HTMLElement).closest("button")) return;
          invoke("begin_drag").catch(() => {});
        }}
        style={{ display: "flex", alignItems: "center", gap: 8, padding: "11px 10px 9px 14px", borderBottom: multi ? "none" : `1px solid ${k.line}`, cursor: pinned ? "grab" : "default" }}>
        <Bolt color={dash.keeper?.alert ? k.amber : k.green} />
        <span style={{ font: `600 13px ${k.ui}`, whiteSpace: "nowrap" }}>Tokenscope</span>
        <div style={{ flex: 1 }} />
        <div style={{ display: "flex", gap: 2 }}>
          {tabs.map((t) => (
            <button key={t.key} onClick={() => setTab(t.key)} style={{
              border: 0, borderRadius: 6, padding: "3px 6px", cursor: "pointer", whiteSpace: "nowrap",
              font: `${tab === t.key ? 600 : 500} 11.5px ${k.ui}`,
              background: tab === t.key ? k.segOn : "transparent",
              color: t.key === "sessions" && hotCount ? k.amber : tab === t.key ? k.text : k.dim,
            }}>{t.label}</button>
          ))}
        </div>
        <button onClick={togglePin} title={pinned ? "Unpin — hide when clicking elsewhere" : "Pin — keep the panel on screen; drag it by this header"}
          style={{ border: 0, borderRadius: 6, width: 24, height: 22, cursor: "pointer", display: "flex", alignItems: "center", justifyContent: "center",
            background: pinned ? k.segOn : "transparent", color: pinned ? k.green : k.dim }}>
          <svg width="13" height="13" viewBox="0 0 24 24" aria-hidden fill="currentColor">
            <path d="M16 3a1 1 0 0 1 .7 1.7L15.4 6l2.6 5.2 1.3-1.3a1 1 0 0 1 1.4 1.4l-3.3 3.3-1.4-1.4-2.3 2.3V20a1 1 0 0 1-1.7.7L9 17.4l-4.3 4.3a1 1 0 0 1-1.4-1.4L7.6 16l-3.3-3a1 1 0 0 1 .7-1.7h4.5l2.3-2.3L10.4 7.6l3.3-3.3A1 1 0 0 1 16 3z" />
          </svg>
        </button>
      </div>
      {multi && (
        <div style={{ display: "flex", gap: 4, padding: "0 14px 8px", borderBottom: `1px solid ${k.line}`, flexWrap: "wrap" }}>
          {[{ dir: "all", name: "All", tok: dash.keeper?.windows.find((w) => w.key === "24h")?.totalTokens ?? 0, alert: dash.keeper?.alert },
            ...accounts.map((a) => ({ dir: a.dir, name: a.name, tok: a.keeper.windows.find((w) => w.key === "24h")?.totalTokens ?? 0, alert: a.keeper.alert }))]
            .map((a) => {
              const on = (acct === "all" && a.dir === "all") || acct === a.dir;
              return (
                <button key={a.dir} onClick={() => pickAcct(a.dir)} title={a.dir === "all" ? "All accounts" : a.dir}
                  style={{ border: `1px solid ${on ? "transparent" : k.line}`, borderRadius: 999, padding: "2px 9px", cursor: "pointer",
                    background: on ? k.segOn : "transparent", color: on ? k.text : k.dim, font: `${on ? 600 : 500} 11px ${k.ui}`,
                    display: "flex", alignItems: "center", gap: 5 }}>
                  {a.alert && <span style={{ width: 6, height: 6, borderRadius: 3, background: k.amber }} />}
                  {a.name}
                  <span style={{ font: `400 10px ${k.mono}`, color: k.faint }}>{fmtTok(a.tok)}</span>
                </button>
              );
            })}
        </div>
      )}
      {!keeper && tab !== "charts" ? (
        <div style={{ padding: 24, font: `400 12px ${k.ui}`, color: k.dim }}>No data yet.</div>
      ) : tab === "usage" ? (
        <UsageTab keeper={keeper!} k={k} toast={toast} onOpenItem={(name, kind) => setSel({ name, kind })} />
      ) : tab === "skills" ? (
        <SkillsTab inv={inv} keeper={keeper!} k={k} busy={busy} acctName={multi && !cur ? (d) => accounts.find((a) => a.dir === d)?.name : undefined} onToggle={toggle} onOpen={(it) => setSel({ name: it.name, kind: it.kind })} />
      ) : tab === "sessions" ? (
        <SessionsTab sessions={keeper!.sessions} k={k} toast={toast} accounts={accounts} />
      ) : (
        <div style={{ flex: 1, minHeight: 0 }}>{children}</div>
      )}
      {sel && keeper && (
        <Detail it={selInv} usage={sel} keeper={keeper} k={k} onClose={() => setSel(null)} onToggle={toggle} onSaved={reload} toast={toast} />
      )}
      {msg && (
        <div style={{ position: "absolute", left: "50%", bottom: 46, transform: "translateX(-50%)", zIndex: 40,
          background: k.raised, color: k.text, border: `1px solid ${k.line}`, borderRadius: 8, padding: "6px 12px",
          font: `500 11.5px ${k.ui}`, boxShadow: "0 6px 20px rgba(0,0,0,0.3)", whiteSpace: "nowrap" }}>{msg}</div>
      )}
    </div>
  );
}
