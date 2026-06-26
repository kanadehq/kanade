// Kanade Client App WebView entry point.
//
// Dashboard redesign: a slim header (brand + a single connection
// status dot), a left sidebar nav (ホーム / 通知 / 端末ヘルス + one
// entry per user-invokable job *category*), and a content area that
// switches views client-side. No framework — the existing IPC + render
// logic is reused as-is; only the layout / navigation is new.
//
// - `client:` jobs (jobs.list, #291) become the category nav items,
//   grouped by their free-form category key (#792) — one tab per
//   distinct key, with operator-supplied label/icon/order.
// - `check:` jobs surface in 端末ヘルス via state.snapshot (#290); each
//   check's 「修復する」 runs its `troubleshoot` job (jobs.execute).
// - Notifications (Phase E, #102) get their own view + sidebar badge.
// - A persistent 実行状況 panel tracks every jobs.execute live from
//   `jobs.progress` pushes regardless of the active view.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { createIcons, icons } from "lucide";
import {
  isPermissionGranted,
  onAction,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

// ---- Lucide icon helpers ----------------------------------------------

// Replace every un-processed `<i data-lucide="…">` placeholder in the
// document with its SVG. Idempotent: already-rendered icons carry no
// `data-lucide`, so re-running only hydrates freshly-injected markup.
function hydrateIcons(): void {
  try {
    createIcons({ icons });
  } catch (e) {
    console.error("lucide hydrate failed", e);
  }
}

// kebab-case (config / data-lucide convention) → PascalCase (the keys of
// lucide's `icons` map), so we can check whether a name is a real icon
// before emitting it (an unknown name would render as an empty box).
function toPascal(name: string): string {
  return name.replace(/(^|-)([a-z0-9])/g, (_, _sep, c: string) =>
    c.toUpperCase(),
  );
}

// Resolve a config-supplied lucide name to a known icon, falling back to
// the category/default icon when it isn't one (so a typo'd `icon:` in a
// job manifest degrades to a sensible glyph instead of a blank).
function lucideName(name: string | null | undefined, fallback: string): string {
  const n = (name ?? "").trim();
  if (n && Object.prototype.hasOwnProperty.call(icons, toPascal(n))) return n;
  return fallback;
}

// Strict allow-list for `data:` icon URLs (a job manifest may ship a
// base64 image instead of a lucide name). Anything not matching this is
// treated as a lucide name — never spliced raw into `src`.
const DATA_ICON_RE =
  /^data:image\/(?:png|jpe?g|gif|webp|svg\+xml);base64,[A-Za-z0-9+/=]+$/;

// Markup for a job/category icon: an inline `<img>` for an allow-listed
// `data:` URL, otherwise a lucide `<i>` placeholder (hydrated later).
function iconHtml(
  icon: string | null | undefined,
  fallback: string,
  cls = "job-icon",
): string {
  const v = (icon ?? "").trim();
  if (DATA_ICON_RE.test(v)) {
    return `<img class="${cls}" src="${v}" alt="" />`;
  }
  // lucideName only ever returns a known-safe lucide slug or the
  // hard-coded fallback, so the attribute value is a fixed charset.
  return `<i class="${cls}" data-lucide="${lucideName(v, fallback)}"></i>`;
}

// ---- IPC types (hand-written; mirror kanade_shared::ipc) ---------------

type CheckStatus = "ok" | "warn" | "fail" | "unknown";

type Check = {
  name: string;
  // Operator-authored display title; when absent we show the `name` slug.
  label?: string | null;
  status: CheckStatus;
  detail?: string | null;
  troubleshoot?: string | null;
};

type StateSnapshot = {
  pc_id: string;
  online: boolean;
  // No `vpn` field: VPN posture, when a site wants it, is an
  // operator-defined `check:` row in `checks` (see kanade_shared
  // StateSnapshot docs), not a built-in snapshot field.
  checks: Check[];
  agent_version: string;
  target_version: string;
};

// Lucide `data-lucide` (kebab) icon per check status — hydrated to an
// SVG by createIcons(). Names verified against the lucide 1.21.0 export
// map; rendered inside a status-tinted badge (see .check-badge in CSS).
const STATUS_ICON: Record<CheckStatus, string> = {
  ok: "circle-check",
  warn: "triangle-alert",
  fail: "circle-x",
  unknown: "circle-help",
};

// Short human label per status — used for the hero count chips and the
// badge's aria-label so color isn't the only signal (a11y).
const STATUS_LABEL: Record<CheckStatus, string> = {
  ok: "正常",
  warn: "注意",
  fail: "異常",
  unknown: "不明",
};

// ---- Jobs / remediation (#291) ----

type RunStatus = "queued" | "running" | "completed" | "failed" | "killed";

type JobProgress = {
  run_id: string;
  status: RunStatus;
  stdout_chunk?: string | null;
  stderr_chunk?: string | null;
  exit_code?: number | null;
};

type JobsExecuteResult = { run_id: string };

// #792: a category is now a free-form key, not a fixed enum.
type JobCategory = string;

type UserInvokableJob = {
  id: string;
  display_name: string;
  display_description?: string | null;
  icon?: string | null;
  category: string;
  // Operator-supplied tab metadata (#792); absent ⇒ fall back to a
  // built-in default for a well-known key, else the key itself.
  category_label?: string | null;
  category_icon?: string | null;
  category_order?: number | null;
  version: string;
};

type JobsListResult = { items: UserInvokableJob[] };

// The raw `RpcNotification` the backend re-emits as a `klp-notification`
// Tauri event; we switch on `method`.
type RpcNotification = { jsonrpc: string; method: string; params: unknown };

type CategoryMeta = { label: string; icon: string; order: number };

// Built-in display defaults for the well-known category keys. Operators
// override per manifest (`client.category_label` / `_icon` / `_order`);
// these are just sensible fallbacks so the common tabs look right with no
// metadata. ANY key now renders a tab — this map is cosmetic, NOT a
// constraint (#792). Orders are spaced so custom tabs slot between/after.
const CATEGORY_DEFAULTS: Record<string, CategoryMeta> = {
  software_update: { label: "アップデート", icon: "download", order: 10 },
  troubleshoot: { label: "困ったとき", icon: "wrench", order: 20 },
  catalog: { label: "カタログ", icon: "package", order: 30 },
};

// Custom keys with no operator order sort after the well-known three.
const CATEGORY_FALLBACK_ORDER = 1000;

// Resolve a category key's tab label / icon / order. Precedence:
// operator metadata carried on the category's jobs (first non-empty) →
// built-in default for a well-known key → the key itself + generic icon.
function categoryMeta(key: string): CategoryMeta {
  const jobs = jobsByCategory.get(key) ?? [];
  const pick = <T>(get: (j: UserInvokableJob) => T | null | undefined): T | undefined => {
    for (const j of jobs) {
      const v = get(j);
      if (v !== null && v !== undefined && String(v).trim() !== "") return v;
    }
    return undefined;
  };
  const def = CATEGORY_DEFAULTS[key];
  return {
    label: (pick((j) => j.category_label) as string)?.trim() || def?.label || key,
    icon: (pick((j) => j.category_icon) as string)?.trim() || def?.icon || "box",
    order: (pick((j) => j.category_order) as number) ?? def?.order ?? CATEGORY_FALLBACK_ORDER,
  };
}

const RUN_STATUS_ICON: Record<RunStatus, string> = {
  queued: "⏳",
  running: "⏳",
  completed: "✅",
  failed: "❌",
  killed: "⏹️",
};

const RUN_STATUS_LABEL: Record<RunStatus, string> = {
  queued: "待機中",
  running: "実行中…",
  completed: "完了しました",
  failed: "失敗しました",
  killed: "中止しました",
};

type Run = {
  runId: string;
  label: string;
  status: RunStatus;
  output: string;
  updatedAt: number;
};

const runs = new Map<string, Run>();
const MAX_RUNS = 30;

// Completed-run ids whose output the user has manually expanded in the
// dock. Running rows always show their output; completed ones collapse to
// the header (a click toggles them here). Mirrors the notifications panel's
// `expandedIds`. A run dropping out of the map is also dropped here on
// evict so the set doesn't leak.
const expandedRunIds = new Set<string>();

function isTerminal(status: RunStatus): boolean {
  return (
    status === "completed" || status === "failed" || status === "killed"
  );
}

function evictOldRuns(): void {
  if (runs.size <= MAX_RUNS) return;
  for (const [id, r] of runs) {
    if (runs.size <= MAX_RUNS) break;
    if (isTerminal(r.status)) {
      runs.delete(id);
      expandedRunIds.delete(id);
    }
  }
}

// Toggle a completed run's output open/closed (running rows always show
// theirs, so this only ever fires on collapsible terminal rows).
function toggleRun(id: string): void {
  if (expandedRunIds.has(id)) expandedRunIds.delete(id);
  else expandedRunIds.add(id);
  renderRuns();
}

// Stuck-run watchdog (see #465): jobs.execute streams no intermediate
// progress, so flag a still-non-terminal run as unresponsive only after
// a deadline safely above any realistic job.
const WATCHDOG_MS = 15 * 60 * 1000;

function checkStuckRuns(): void {
  const now = Date.now();
  let changed = false;
  for (const r of runs.values()) {
    if (!isTerminal(r.status) && now - r.updatedAt > WATCHDOG_MS) {
      r.status = "failed";
      r.output +=
        (r.output ? "\n" : "") +
        "⏱ 応答がありません（エージェントが停止しているか、想定より長くかかっています）";
      changed = true;
    }
  }
  if (changed) renderRuns();
}

// Briefly highlight the (now sticky) 実行状況 panel so a launched job
// gives an unmistakable "it started" signal — guards against re-pressing
// 実行 when the confirmation would otherwise be easy to miss.
function flashRunsPanel(): void {
  const section = $("runs-section");
  if (section.hidden) return;
  section.classList.remove("runs-flash");
  void section.offsetWidth; // restart the animation
  section.classList.add("runs-flash");
}

async function executeJob(jobId: string, label: string): Promise<void> {
  try {
    const r = await invoke<JobsExecuteResult>("jobs_execute", { id: jobId });
    const existing = runs.get(r.run_id);
    if (existing) {
      existing.label = label;
      existing.updatedAt = Date.now();
    } else {
      runs.set(r.run_id, {
        runId: r.run_id,
        label,
        status: "running",
        output: "",
        updatedAt: Date.now(),
      });
    }
    renderRuns();
    flashRunsPanel();
  } catch (err) {
    const pseudoId = `error-${jobId}-${runs.size}`;
    runs.set(pseudoId, {
      runId: pseudoId,
      label,
      status: "failed",
      output: `実行できませんでした: ${String(err)}`,
      updatedAt: Date.now(),
    });
    renderRuns();
    flashRunsPanel();
  }
}

async function killRun(runId: string): Promise<void> {
  try {
    await invoke("jobs_kill", { runId });
  } catch (err) {
    console.error("jobs_kill failed", err);
    const r = runs.get(runId);
    if (r) {
      r.output += `\n中止リクエストに失敗しました（既に終了している可能性があります）: ${String(err)}`;
      renderRuns();
    }
  }
}

function handleProgress(p: JobProgress): void {
  const existing = runs.get(p.run_id);
  const run: Run = existing ?? {
    runId: p.run_id,
    label: p.run_id,
    status: p.status,
    output: "",
    updatedAt: Date.now(),
  };
  run.status = p.status;
  run.updatedAt = Date.now();
  // #806: a Running push carries an incremental delta — append it for the
  // live view. A terminal push carries the authoritative FULL output, so
  // REPLACE the streamed-so-far buffer with it; that self-heals any live
  // delta that was dropped or duplicated. (Pre-#806 the agent sent no
  // running-body, so replace == append == full — unchanged for old agents.)
  if (isTerminal(p.status)) {
    const full = (p.stdout_chunk ?? "") + (p.stderr_chunk ?? "");
    // Don't let the terminal push DESTROY output we already streamed:
    //  - the terminal is capped at 256 KiB and keeps only the HEAD
    //    (marked "truncated"), whereas the live stream carried the real
    //    tail — keep the longer streamed buffer in that case;
    //  - a terminal with no body at all must not wipe a non-empty stream.
    // Otherwise the terminal is authoritative and replaces.
    const lossy = full.includes("truncated: output exceeded") || full === "";
    if (!(lossy && run.output.length > full.length)) {
      run.output = full;
    }
  } else {
    if (p.stdout_chunk) run.output += p.stdout_chunk;
    if (p.stderr_chunk) run.output += p.stderr_chunk;
  }
  runs.set(p.run_id, run);
  renderRuns();
}

function activeRunCount(): number {
  let n = 0;
  for (const r of runs.values()) if (!isTerminal(r.status)) n++;
  return n;
}

// Dock row order: active (running / queued) runs first so they're always
// at the top of the scroll and never pushed out of view by a pile of
// completed ones, then terminal runs — each group newest-first.
function orderedRuns(): Run[] {
  const all = [...runs.values()];
  const active = all.filter((r) => !isTerminal(r.status)).reverse();
  const terminal = all.filter((r) => isTerminal(r.status)).reverse();
  return [...active, ...terminal];
}

function renderRuns(): void {
  evictOldRuns();
  const section = $("runs-section");
  section.hidden = runs.size === 0;
  if (runs.size > 0) {
    const list = orderedRuns();
    const container = $("runs");
    // Full render whenever the row set OR its order changes (a run going
    // terminal moves it from the active group to the terminal one); only
    // then do we accept the reflow. Otherwise update each row in place so a
    // status/output tick doesn't blow away scroll / text selection. The
    // order check also subsumes the "same length, different keys" case (an
    // evict + add in one pass) the length check alone would miss.
    // Compare against the RAW runId: `el.id` returns the browser-decoded
    // value, so a runId with an HTML-special char (`&` etc.) would never
    // match the escaped form and force a full re-render every tick. The
    // `getElementById` lookup below is already keyed on the raw id.
    const domIds = [...container.children].map((el) => el.id);
    const wantIds = list.map((r) => `run-${r.runId}`);
    const sameOrder =
      domIds.length === wantIds.length &&
      domIds.every((id, i) => id === wantIds[i]);
    if (!sameOrder) {
      container.innerHTML = list.map(renderRun).join("");
    } else {
      for (const r of list) {
        const el = document.getElementById(`run-${r.runId}`);
        if (!el) continue;
        const tmp = document.createElement("div");
        tmp.innerHTML = renderRun(r);
        const fresh = tmp.firstElementChild;
        if (fresh) el.replaceWith(fresh);
      }
    }
  }
  updateRunsActiveBadge();
  updateRunsCard();
}

// Live 実行中 count in the fixed dock header (visible regardless of how far
// the row list is scrolled).
function updateRunsActiveBadge(): void {
  const active = activeRunCount();
  const badge = $("runs-active-badge");
  badge.hidden = active === 0;
  badge.textContent = `実行中 ${active}`;
}

// #793: sentinel lines fencing an inventory JSON payload inside an
// otherwise human-readable job stdout. The backend projector parses the
// fenced region; here we strip it so the user only sees the message. Must
// mirror kanade_shared::manifest::INVENTORY_BLOCK_{BEGIN,END} (Rust source
// of truth) — keep both in sync.
// One pair per fenced hint (#821) — keep in sync with
// kanade_shared::manifest::{INVENTORY,CHECK,COLLECT}_BLOCK_{BEGIN,END}.
const HINT_BLOCKS: ReadonlyArray<readonly [string, string]> = [
  ["#KANADE-INVENTORY-BEGIN", "#KANADE-INVENTORY-END"],
  ["#KANADE-CHECK-BEGIN", "#KANADE-CHECK-END"],
  ["#KANADE-COLLECT-BEGIN", "#KANADE-COLLECT-END"],
];

// Find a marker only where it begins a line (start of string or right
// after a "\n"), mirroring kanade_shared::manifest::find_line_marker — so a
// sentinel echoed mid-message can't false-trigger the strip.
function findLineMarker(hay: string, needle: string): number {
  if (hay.startsWith(needle)) return 0;
  const p = hay.indexOf("\n" + needle);
  return p === -1 ? -1 : p + 1;
}

// Remove one fenced block. An unterminated fence (closing marker not yet
// streamed in, #806) hides everything from the opener onward, so the raw
// JSON never flashes mid-stream. No fence → returned unchanged.
function stripOneBlock(s: string, begin: string, end: string): string {
  const b = findLineMarker(s, begin);
  if (b === -1) return s;
  const afterBegin = b + begin.length;
  const endRel = findLineMarker(s.slice(afterBegin), end);
  const cut = endRel === -1 ? s.length : afterBegin + endRel + end.length;
  return s.slice(0, b) + s.slice(cut);
}

// Strip every hint JSON block from job output before showing it to the
// user — the blocks are for the projector / agent, not the human. #821:
// a single job may carry inventory, check, AND collect blocks at once.
function stripHintBlocks(s: string): string {
  let out = s;
  for (const [begin, end] of HINT_BLOCKS) out = stripOneBlock(out, begin, end);
  // `\r?\n` so the collapse also works on Windows (CRLF) job output.
  return out.replace(/(?:\r?\n){3,}/g, "\n\n").trim();
}

function renderRun(r: Run): string {
  const icon = RUN_STATUS_ICON[r.status] ?? "⏳";
  const label = RUN_STATUS_LABEL[r.status] ?? r.status;
  const active = !isTerminal(r.status);
  // Show the human-readable part only — the fenced JSON blocks (if any)
  // are for the projector / agent, not the user.
  const shown = stripHintBlocks(r.output);
  const hasOutput = !!shown.trim();
  // Running rows always show output; completed rows collapse it behind a
  // chevron and only render it when the user has expanded the row.
  const collapsible = !active && hasOutput;
  const expanded = active || expandedRunIds.has(r.runId);
  const id = escapeHtml(r.runId);

  const kill = active
    ? `<button class="kill-btn" data-run-id="${id}">中止</button>`
    : "";
  const chevron = collapsible
    ? `<span class="run-chevron" aria-hidden="true">▸</span>`
    : "";
  const toggleAttrs = collapsible
    ? ` data-run-toggle="${id}" role="button" tabindex="0" aria-expanded="${expanded}"`
    : "";
  const output =
    hasOutput && expanded
      ? `<pre class="run-output">${escapeHtml(shown.slice(-4000))}</pre>`
      : "";

  return `
    <div class="run-row status-${escapeHtml(r.status)}${expanded ? " expanded" : ""}" id="run-${id}">
      <div class="run-head"${toggleAttrs}>
        <span class="run-icon">${icon}</span>
        <span class="run-name">${escapeHtml(r.label)}</span>
        <span class="run-status muted">${escapeHtml(label)}</span>
        ${kill}
        ${chevron}
      </div>
      ${output}
    </div>`;
}

// ---- Job catalog (#291): category nav + per-category job list ----

const jobsByCategory = new Map<JobCategory, UserInvokableJob[]>();
// Set when a category tab is opened (#792: categories are dynamic, so
// there's no fixed first tab to default to).
let activeJobsTab: JobCategory = "";
let jobsLoaded = false;

async function loadJobs(): Promise<void> {
  if (jobsLoaded) return;
  jobsLoaded = true;
  try {
    // `category: null` → every tab's jobs in one round-trip; group locally.
    const res = await invoke<JobsListResult>("jobs_list", { category: null });
    jobsByCategory.clear();
    for (const job of res.items) {
      const list = jobsByCategory.get(job.category) ?? [];
      list.push(job);
      jobsByCategory.set(job.category, list);
    }
    renderCategoryNav();
    // Keep the current jobs view in sync if one is open.
    if (activeView === "jobs") renderJobsList();
  } catch (err) {
    jobsLoaded = false; // let a later (re)connect retry
    $("nav-categories").innerHTML =
      `<p class="nav-error error">ジョブ一覧を取得できません</p>`;
    console.error("jobs_list failed", err);
  }
}

// Distinct categories that actually have jobs, each with its resolved tab
// metadata, sorted by (order, label) for a stable layout (#792). Shared by
// the nav and the active-tab fallback so both order identically.
function sortedCategories(): (CategoryMeta & { key: string })[] {
  return [...jobsByCategory.keys()]
    .filter((k) => (jobsByCategory.get(k)?.length ?? 0) > 0)
    .map((key) => ({ key, ...categoryMeta(key) }))
    .sort((a, b) => a.order - b.order || a.label.localeCompare(b.label));
}

// Inject one sidebar entry per category that actually has jobs (#792:
// tabs are driven by the distinct category keys present, not a fixed list).
function renderCategoryNav(): void {
  const cats = sortedCategories();
  $("nav-jobs-sep").hidden = cats.length === 0;
  $("nav-categories").innerHTML = cats
    .map((m) => {
      const count = jobsByCategory.get(m.key)?.length ?? 0;
      return `
        <button class="nav-item" data-view="jobs" data-category="${escapeHtml(m.key)}">
          ${iconHtml(m.icon, m.icon, "nav-icon")}
          <span class="nav-label">${escapeHtml(m.label)}</span>
          <span class="nav-count muted">${count}</span>
        </button>`;
    })
    .join("");
  hydrateIcons();
}

function renderJobsList(): void {
  // If the jobs view is shown without a tab ever having been clicked
  // (e.g. a reconnect re-render), fall back to the first tab instead of an
  // empty-state for a phantom category (Claude #811).
  if (!jobsByCategory.has(activeJobsTab)) {
    const first = sortedCategories()[0];
    if (first) activeJobsTab = first.key;
  }
  const meta = categoryMeta(activeJobsTab);
  $("jobs-view-title").textContent = meta.label;
  const jobs = jobsByCategory.get(activeJobsTab) ?? [];
  if (jobs.length === 0) {
    $("jobs-list").innerHTML =
      `<p class="muted">このカテゴリのジョブはありません</p>`;
    return;
  }
  $("jobs-list").innerHTML = jobs.map(renderJobRow).join("");
  hydrateIcons();
}

function renderJobRow(j: UserInvokableJob): string {
  const fallback = categoryMeta(j.category).icon;
  const desc = j.display_description
    ? `<span class="job-desc">${escapeHtml(j.display_description)}</span>`
    : "";
  return `
    <div class="job-row">
      ${iconHtml(j.icon, fallback)}
      <span class="job-name">${escapeHtml(j.display_name)}</span>
      <button class="job-run-btn" data-job-id="${escapeHtml(j.id)}" data-label="${escapeHtml(j.display_name)}">実行</button>
      ${desc}
    </div>`;
}

// ---- Notifications (Phase E, #102) ----

type NotificationPriority = "info" | "warn" | "emergency" | "unknown";

type AppNotification = {
  id: string;
  priority: NotificationPriority;
  require_ack: boolean;
  title: string;
  body: string;
  // Whether to surface an OS toast — driven by this flag, NOT by priority.
  // true = persistent native toast (+ agent launches us when closed, lock
  // screen / Action Center, re-pop on unlock); false = in-app panel only.
  toast: boolean;
  issued_at: string;
  issued_by?: string | null;
  expires_at?: string | null;
  acked_at?: string | null;
  // Set when the notification was edited in place (re-published with the same
  // id + issued_at but new content). Used to recognise an edit vs a fresh send.
  edited_at?: string | null;
  // When an edit reset confirmations: a local ack older than this is stale and
  // the user must re-confirm the new content.
  acks_reset_at?: string | null;
};

type NotificationsListResult = {
  items: AppNotification[];
  next_cursor?: string | null;
};

type NotificationsAckResult = { acked_at: string };

// `notifications.amended` push: a post-send operation on a notification this
// client may be showing. Currently only `recall` (the operator deleted it) —
// the `op.kind` tag leaves room for a future `update`.
type NotificationAmend = {
  id: string;
  op: { kind: "recall" };
};

const PRIORITY_ICON: Record<NotificationPriority, string> = {
  info: "ℹ️",
  warn: "⚠️",
  emergency: "🚨",
  unknown: "🔔",
};

const PRIORITY_LABEL: Record<NotificationPriority, string> = {
  info: "情報",
  warn: "警告",
  emergency: "緊急",
  unknown: "通知",
};

const notifications = new Map<string, AppNotification>();
// Ids whose body is expanded in the panel. A notification renders collapsed
// (title only) until the user clicks it open — opening an unread, ack-optional
// notification is what marks it read (the user can't have "seen" a body that
// was never expanded). Survives re-renders.
const expandedIds = new Set<string>();
// Ids whose body the user has expanded at least once. An ack-required
// notification's 確認 stays locked until its body has been opened (read), so
// the user can't confirm content they never saw. Unlike `expandedIds` this is
// sticky across a re-collapse — re-hiding the body doesn't re-lock 確認.
const seenIds = new Set<string>();
let notifSubscribed = false;

function isExpired(n: AppNotification): boolean {
  if (!n.expires_at) return false;
  const t = Date.parse(n.expires_at);
  return !Number.isNaN(t) && t <= Date.now();
}

async function loadNotifications(): Promise<void> {
  if (!notifSubscribed) {
    try {
      await invoke("notifications_subscribe");
      notifSubscribed = true;
    } catch (err) {
      console.error("notifications_subscribe failed", err);
      return;
    }
  }
  try {
    const res = await invoke<NotificationsListResult>("notifications_list", {
      filter: "all",
      cursor: null,
    });
    for (const n of res.items) notifications.set(n.id, n);
    // Reconcile recalls missed while offline: an id deleted from the stream
    // no longer comes back in `notifications.list`, so any locally-held id
    // absent from a COMPLETE listing was recalled — drop it. Guard on
    // `next_cursor`: if the history is paginated (more pages exist) an id
    // beyond this first page isn't really gone, so skip the prune entirely
    // rather than wrongly evicting it. Expired ids are handled separately by
    // `evictOldNotifications`, so leave them be here.
    if (!res.next_cursor) {
      const present = new Set(res.items.map((n) => n.id));
      for (const [id, n] of notifications) {
        if (!present.has(id) && !isExpired(n)) {
          notifications.delete(id);
          expandedIds.delete(id);
          seenIds.delete(id);
        }
      }
    }
    renderNotifications();

    // Toast-click launch (#647): this process was started by clicking an
    // emergency toast (`kanade-client://show?id=<id>`), the window is visible —
    // scroll to + flash that notification once.
    if (!launchFocusFetched) {
      launchFocusFetched = true;
      try {
        const focusId = (await invoke<string | null>("get_launch_focus")) ?? null;
        if (focusId) focusNotificationInPanel(focusId);
      } catch (err) {
        console.error("get_launch_focus failed", err);
      }
    }

    await ensureLaunchId();
    const target = pendingNotificationId
      ? notifications.get(pendingNotificationId)
      : null;
    if (pendingNotificationId && target) {
      // Agent-launched for a specific emergency (#102): surface it as a
      // native OS toast (the window stays hidden so it never bursts over
      // a meeting) — clicking the toast opens the window onto it.
      if (
        !target.acked_at &&
        !isExpired(target) &&
        !toastedIds.has(target.id)
      ) {
        void surfaceOsToast(target);
      } else {
        // Already acked / expired / toasted — reveal the window so the
        // agent-launch isn't a silent no-op.
        void invoke("show_main_window").catch(() => {});
      }
    } else if (pendingNotificationId) {
      // Launched for an id we don't have in history — reveal the window.
      void invoke("show_main_window").catch(() => {});
    } else {
      // Normal / reconnect launch (SPEC §2.12.8 recovery): an unread,
      // non-expired toast notification whose live push arrived while the
      // pipe was down (so we never toasted it) is surfaced now from
      // history. The `toastedIds` guard keeps a reconnect from re-toasting
      // one already shown.
      for (const n of res.items) {
        if (
          n.toast &&
          !n.acked_at &&
          !isExpired(n) &&
          !toastedIds.has(n.id)
        ) {
          void surfaceOsToast(n);
        }
      }
    }
  } catch (err) {
    console.error("notifications_list failed", err);
  }
}

// ---- Toast-fallback launch (#102): agent → `--show-notification <id>` ----

let pendingNotificationId: string | null = null;
let launchIdFetched = false;
// Whether we've consumed a toast-click launch focus id (#647) — fetched once.
let launchFocusFetched = false;
// Ids we've already shown an OS toast for, so the reconnect-recovery loop
// (and the agent-launch path) never re-toast the same notification.
const toastedIds = new Set<string>();
// `onAction` is registered at most once for the app's lifetime.
let toastActionRegistered = false;
// The notification a toast click should focus — the most-recently sent
// one (onAction doesn't reliably carry our id back, so we track it).
let lastToastNotifId: string | null = null;

async function ensureLaunchId(): Promise<void> {
  if (launchIdFetched) return;
  launchIdFetched = true;
  try {
    pendingNotificationId =
      (await invoke<string | null>("get_launch_notification")) ?? null;
  } catch (err) {
    console.error("get_launch_notification failed", err);
    pendingNotificationId = null;
  }
}

// Surface a notification as a native OS toast — the whole point of OS
// toasts (vs an in-app toast or a modal) is that they show in Windows'
// notification area regardless of whether our window is visible/focused,
// so they never burst over whatever the user is doing (a meeting).
// Surfacing is driven by the `toast` flag, NOT by priority: a
// notification with toast:false stays in the in-app panel only and this is
// a no-op. Clicking the toast reveals the window focused on the
// notification in the panel (where its 確認 button lives). Falls back to
// revealing the window if toast permission is denied or the OS-toast call
// fails — so a toast notification is never silently lost.
async function surfaceOsToast(n: AppNotification): Promise<void> {
  if (!n.toast) return;
  const icon = PRIORITY_ICON[n.priority] ?? PRIORITY_ICON.unknown;
  // Mark as toasted up front so the reconnect-recovery loop won't
  // re-toast it (and a concurrent live push for the same id is a no-op).
  toastedIds.add(n.id);

  // toast:true → native WinRT toast (show_emergency_toast): it persists on
  // screen until dismissed (scenario=Reminder) and stays in the Action
  // Center, unlike the plugin's sendNotification which auto-dismisses in
  // ~7s. Clicking it (body or 確認 button) opens the client focused on this
  // notification via the kanade-client:// protocol (#647) — hence the id.
  // Fall through to the plugin path only if the native command fails.
  try {
    await invoke("show_emergency_toast", {
      title: `${icon} ${n.title}`,
      body: n.body,
      id: n.id,
    });
    return;
  } catch (err) {
    console.error("native toast failed; falling back to plugin toast", err);
  }

  try {
    let granted = await isPermissionGranted();
    if (!granted) granted = (await requestPermission()) === "granted";
    if (!granted) {
      revealForToast(n);
      return;
    }
    if (!toastActionRegistered) {
      // Fires when the user clicks any OS toast this app sent. onAction
      // doesn't reliably carry our notification id, so it focuses the
      // most-recently sent one (`lastToastNotifId`).
      //
      // BEST-EFFORT: `onAction` is mobile-only in tauri-plugin-notification
      // — on desktop its backing command isn't registered, so the call
      // REJECTS. That rejection must NOT abort the toast: it sits before
      // `sendNotification` below, so an unguarded `await` here threw us into
      // the catch (revealForToast) and NO toast was ever sent — the toast
      // silently fell back to a window on every desktop launch.
      // Mark it attempted up front (don't retry the unsupported call on
      // every toast) and swallow the desktop rejection; toast-click focus
      // just isn't available on desktop.
      toastActionRegistered = true;
      try {
        await onAction(() => {
          void openFromToast();
        });
      } catch (e) {
        console.warn(
          "notification onAction unavailable on this platform; toast-click focus disabled",
          e,
        );
      }
    }
    // Set the click target as close to the actual send as possible (after
    // the awaits) so a burst of concurrent toasts leaves the
    // most-recently-SENT id here, not whichever call resolved last.
    lastToastNotifId = n.id;
    sendNotification({ title: `${icon} ${n.title}`, body: n.body });
  } catch (err) {
    console.error("OS toast failed", err);
    revealForToast(n);
  }
}

// Fallback when the OS toast can't be shown (permission denied / failure):
// reveal the window ONLY for a toast notification — stealing focus for a
// non-toast info/warn would defeat the non-intrusive goal (it's still in
// the panel).
function revealForToast(n: AppNotification): void {
  if (!n.toast) return;
  void invoke("show_main_window").catch(() => {});
  focusNotificationInPanel(n.id);
}

// Toast clicked → reveal + focus the window, scroll to the notification.
async function openFromToast(): Promise<void> {
  await invoke("show_main_window").catch(() => {});
  if (lastToastNotifId) focusNotificationInPanel(lastToastNotifId);
}

// Scroll the notification view to a notification and briefly flash it.
// Switches to the notifications view first so the row is on screen.
function focusNotificationInPanel(id: string): void {
  showView("notifications");
  // Jumping to a notification reveals its body — same as opening it by hand,
  // so mark an ack-optional one read here too (otherwise the unread dot
  // lingers while the user is looking straight at the body). `seenIds` too, so
  // a toast-opened ack-required row can be confirmed straight away.
  expandedIds.add(id);
  seenIds.add(id);
  const focused = notifications.get(id);
  if (focused && !focused.acked_at && !focused.require_ack) {
    void ackNotification(id).catch(() => {});
  }
  renderNotifications();
  const el = document.getElementById(`cnotif-${id}`);
  if (!el) return;
  el.scrollIntoView({ behavior: "smooth", block: "center" });
  // Only flash ack-required rows: an ack-optional one was just ack'd above,
  // and that async re-render replaces this node and would cut the animation
  // short (the expand is its own visual signal anyway).
  if (focused?.require_ack) {
    el.classList.add("notif-flash");
    window.setTimeout(() => el.classList.remove("notif-flash"), 2000);
  }
}

// Apply one `notifications.new` push: store, re-render, and (when
// `toast:true`) surface as a non-intrusive OS toast (no screen-grabbing
// modal, no in-app toast that only shows when the window is up).
// `surfaceOsToast` is a no-op for `toast:false` (in-app panel only).
//
// An EDIT (`PATCH /api/notifications/{id}`) re-publishes the notification with
// the SAME id + issued_at, so it arrives here too. We treat that as a content
// update of a notification the user already holds, NOT a fresh arrival:
//  - don't re-toast a typo fix — only surface when the edit NEWLY enabled toast
//    (false→true) AND this user hasn't confirmed it yet (deliberate "make it
//    pop for people who haven't dealt with it");
//  - preserve this user's local ack across the replace (the re-published copy
//    carries acked_at:null), UNLESS the edit reset confirmations
//    (`acks_reset_at` newer than the local ack) — then it goes back to unread.
function handleNewNotification(incoming: AppNotification): void {
  const existing = notifications.get(incoming.id);
  const isEdit = !!existing && existing.issued_at === incoming.issued_at;

  let n = incoming;
  if (isEdit && existing) {
    const resetAt = incoming.acks_reset_at ? Date.parse(incoming.acks_reset_at) : NaN;
    const localAck = existing.acked_at ? Date.parse(existing.acked_at) : NaN;
    const ackInvalidated =
      !Number.isNaN(resetAt) && (Number.isNaN(localAck) || localAck < resetAt);
    // Carry the local ack forward unless the reset invalidated it.
    if (!ackInvalidated && existing.acked_at) {
      n = { ...incoming, acked_at: existing.acked_at };
    }
    // A confirmation-reset edit puts the notification back to unread with NEW
    // content — drop the sticky read/expanded state so 確認 re-locks to
    // 「開いて確認」 and the user must re-open the edited body before confirming
    // (otherwise the stale `seenIds` entry would leave 確認 unlocked).
    if (ackInvalidated) {
      seenIds.delete(incoming.id);
      expandedIds.delete(incoming.id);
    }
  }

  notifications.set(n.id, n);
  renderNotifications();
  if (isExpired(n)) return;

  if (isEdit && existing) {
    // Silent content update by default. Only surface when this edit turned
    // toast ON and the user hasn't (re-)confirmed it.
    const newlyEnabledToast = n.toast && !existing.toast;
    if (newlyEnabledToast && !n.acked_at) void surfaceOsToast(n);
    return;
  }

  // Genuinely new arrival.
  void surfaceOsToast(n);
}

// Apply one `notifications.amended` push. Currently only `recall`: the
// operator deleted the notification, so drop it from the panel + unread count.
// An id we never held is a no-op (this is a single fleet-wide broadcast, so
// every client sees every recall and filters by what it has). An OS toast
// already surfaced in the Action Center can't be programmatically dismissed —
// only the in-app panel clears.
function handleAmendNotification(a: NotificationAmend): void {
  if (a.op?.kind !== "recall") return;
  if (!notifications.has(a.id)) return;
  notifications.delete(a.id);
  expandedIds.delete(a.id);
  seenIds.delete(a.id);
  renderNotifications();
}

// Single-instance forward (#624): a SECOND `kanade-client` launched with
// `--show-notification <id>` (the agent's no-subscriber toast fallback) was
// collapsed into this already-running instance, which forwarded the id
// here. Toast that notification from here so a new hidden process never
// piles up. If we already toasted it (its live push beat the forward), do
// nothing — `surfaceOsToast` only *records* into `toastedIds`, it doesn't
// guard on it, so the caller must (else we'd double-toast). If we don't
// have it yet (forwarded before its push), re-pull history once and
// retry; failing that, reveal the window so the forward isn't a silent
// no-op.
async function surfaceForwardedToast(id: string): Promise<void> {
  if (toastedIds.has(id)) return; // already surfaced by the live push
  const tryToast = (): boolean => {
    const n = notifications.get(id);
    if (n && !isExpired(n) && !n.acked_at) {
      void surfaceOsToast(n);
      return true;
    }
    return false;
  };
  if (tryToast()) return;
  try {
    const res = await invoke<NotificationsListResult>("notifications_list", {
      filter: "all",
      cursor: null,
    });
    for (const n of res.items) notifications.set(n.id, n);
    renderNotifications();
  } catch (err) {
    console.error("forwarded toast notification: list re-pull failed", err);
  }
  if (!tryToast()) {
    void invoke("show_main_window").catch(() => {});
  }
}

// Presence-driven re-surface (#647): the agent forwarded `--resurface` because
// the user just became present (logon / unlock) after toast notifications they
// couldn't see — sent while signed out, or delivered to the Action Center
// while the screen was locked. Re-pull the freshest unread set, then re-toast
// every unread, unexpired `toast:true` notification, DELIBERATELY bypassing the
// toastedIds dedup (the whole point is to re-pop ones already silently
// delivered). `toast:false` ones stay passive.
async function resurfaceAllToasts(): Promise<void> {
  try {
    const res = await invoke<NotificationsListResult>("notifications_list", {
      filter: "all",
      cursor: null,
    });
    for (const n of res.items) notifications.set(n.id, n);
    // Reconcile recalls (same guard as `loadNotifications`): a notification
    // recalled while the screen was locked is gone from the stream but still
    // in the map, and this function re-toasts every map entry — so without
    // this prune a recalled notification would re-pop on unlock. Only prune on
    // a COMPLETE listing (`next_cursor` absent) so a paginated-out id isn't
    // wrongly evicted. `klp-connected`/`loadNotifications` does NOT fire on a
    // plain unlock of an already-connected session, so this is the only place
    // that catches a lock-window recall before the re-toast loop.
    if (!res.next_cursor) {
      const present = new Set(res.items.map((n) => n.id));
      for (const [id, n] of notifications) {
        if (!present.has(id) && !isExpired(n)) {
          notifications.delete(id);
          expandedIds.delete(id);
          seenIds.delete(id);
        }
      }
    }
    renderNotifications();
  } catch (err) {
    console.error("resurface: list re-pull failed", err);
  }
  for (const n of notifications.values()) {
    if (n.toast && !n.acked_at && !isExpired(n)) {
      void surfaceOsToast(n);
    }
  }
}

// Ack a notification for this OS user. Marks it read locally on success;
// the panel re-renders to swap the 確認 button for the ✓確認済み marker.
async function ackNotification(id: string): Promise<void> {
  try {
    const r = await invoke<NotificationsAckResult>("notifications_ack", { id });
    const n = notifications.get(id);
    if (n) n.acked_at = r.acked_at;
    renderNotifications();
  } catch (err) {
    console.error("notifications_ack failed", err);
    throw err;
  }
}

// Count unread, non-expired notifications (drives the sidebar badge +
// dashboard card).
function unreadCount(): number {
  let n = 0;
  for (const x of notifications.values()) {
    if (!isExpired(x) && !x.acked_at) n++;
  }
  return n;
}

function renderNotifications(): void {
  evictOldNotifications();
  const live = [...notifications.values()]
    .filter((n) => !isExpired(n))
    .sort((a, b) => Date.parse(b.issued_at) - Date.parse(a.issued_at));
  const container = $("notifications");
  if (live.length === 0) {
    container.innerHTML = `<p class="muted">通知はありません</p>`;
  } else {
    container.innerHTML = live.map(renderNotification).join("");
  }
  updateNotifBadges();
}

// Reflect the unread count onto the view-title badge, the sidebar nav
// badge, and the dashboard card.
function updateNotifBadges(): void {
  const unread = unreadCount();
  for (const id of ["notif-badge", "nav-notif-badge"]) {
    const badge = $(id);
    badge.hidden = unread === 0;
    badge.textContent = String(unread);
  }
  $("card-notif-value").textContent =
    unread > 0 ? `${unread}件の未読` : "未読なし";
}

const MAX_NOTIFICATIONS = 100;

function evictOldNotifications(): void {
  // Expired notifications are filtered out of the panel but otherwise linger
  // in the map (and expandedIds) forever — drop them outright so a
  // long-running session doesn't leak them.
  for (const [id, n] of notifications) {
    if (isExpired(n)) {
      notifications.delete(id);
      expandedIds.delete(id);
      seenIds.delete(id);
    }
  }
  if (notifications.size <= MAX_NOTIFICATIONS) return;
  for (const [id, n] of notifications) {
    if (notifications.size <= MAX_NOTIFICATIONS) break;
    if (n.acked_at) {
      notifications.delete(id);
      expandedIds.delete(id);
      seenIds.delete(id);
    }
  }
}

// One-line body preview (~120 chars) for the collapsed state — single source
// of truth for "this notification has a body, click to read it". Returns "" for
// an empty body. The CSS clamps it to one line + ellipsis; the slice just keeps
// the DOM small for a very long body.
function notifPreview(body: string): string {
  const flat = body.replace(/\s+/g, " ").trim();
  if (!flat) return "";
  // Slice by Unicode code points (`Array.from`) — `String.slice` cuts UTF-16
  // code units and would split a surrogate pair (emoji / rare kanji) at the
  // boundary, leaving a broken glyph.
  const sliced = Array.from(flat).slice(0, 120).join("");
  return escapeHtml(sliced);
}

function renderNotification(n: AppNotification): string {
  const icon = PRIORITY_ICON[n.priority] ?? PRIORITY_ICON.unknown;
  const acked = !!n.acked_at;
  const expanded = expandedIds.has(n.id);
  const id = escapeHtml(n.id);
  const meta = [
    n.issued_by ? `送信元: ${escapeHtml(n.issued_by)}` : "",
    fmtTime(n.issued_at),
  ]
    .filter(Boolean)
    .join(" · ");
  // Ack-required notifications clear their unread state via the explicit
  // 確認 button; ack-optional ones clear it by being opened (read). So only
  // require_ack carries an action control here. The 確認 button is GATED on
  // having opened the body: until then it's a 「開いて確認」 button that just
  // expands the row (data-notif-toggle), so the user can't confirm content
  // they never read. Once read it becomes the real 確認 (data-notif-id → ack).
  const seen = seenIds.has(n.id);
  const ackCtl = !n.require_ack
    ? ""
    : acked
      ? `<span class="notif-acked muted">✓ 確認済み</span>`
      : seen
        ? `<button class="notif-ack-btn" data-notif-id="${id}">確認</button>`
        : `<button class="notif-ack-btn notif-ack-locked" data-notif-toggle="${id}" title="本文を開いて内容をご確認ください">開いて確認</button>`;
  // An unread dot makes the badge count legible per-row; it clears the
  // moment the notification is read/acked.
  const unreadDot = acked ? "" : `<span class="notif-dot" aria-hidden="true"></span>`;
  // Collapsed preview: makes it unmistakable that there IS a body to read and
  // that the row expands. Hidden via CSS once expanded (the full text shows).
  const preview = notifPreview(n.body);
  const previewEl = preview
    ? `<p class="notif-preview notif-toggle" data-notif-toggle="${id}">${preview}</p>`
    : "";
  const classes = [
    "notif-row",
    `priority-${escapeHtml(n.priority)}`,
    acked ? "acked" : "unread",
    expanded ? "expanded" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return `
    <div id="cnotif-${id}" class="${classes}">
      <span class="notif-icon">${icon}</span>
      <div class="notif-main">
        <div class="notif-head notif-toggle" data-notif-toggle="${id}" role="button" tabindex="0" aria-expanded="${expanded}">
          ${unreadDot}
          <span class="notif-title">${escapeHtml(n.title)}</span>
          <span class="notif-prio muted">${escapeHtml(PRIORITY_LABEL[n.priority] ?? PRIORITY_LABEL.unknown)}</span>
          <span class="notif-chevron" aria-hidden="true">▸</span>
        </div>
        ${previewEl}
        <div class="notif-collapse">
          <p class="notif-text">${escapeHtml(n.body)}</p>
          <p class="notif-meta muted">${meta}</p>
        </div>
      </div>
      ${ackCtl}
    </div>`;
}

// Toggle a notification's body open/closed. Opening an unread, ack-optional
// notification marks it read (the "seen" signal the user asked for — a body
// you never expanded can't have been read). Ack-required ones still need the
// explicit 確認 button, so opening them only reveals the body.
function toggleNotification(id: string): void {
  if (expandedIds.has(id)) {
    expandedIds.delete(id);
  } else {
    expandedIds.add(id);
    // Sticky "the user has read this" flag — unlocks 確認 for ack-required
    // rows even after a later re-collapse.
    seenIds.add(id);
    const n = notifications.get(id);
    if (n && !n.acked_at && !n.require_ack) {
      void ackNotification(id).catch(() => {});
    }
  }
  renderNotifications();
}

// Drop expired notifications from the panel as their `expires_at` passes,
// even while the app sits idle (a notification can expire with nothing
// else pushing a re-render). All surfacing is via OS toasts now — there's
// no in-app toast or blocking modal to tear down.
function sweepExpired(): void {
  renderNotifications();
}

function fmtTime(iso: string): string {
  const d = new Date(iso);
  return escapeHtml(Number.isNaN(d.getTime()) ? iso : d.toLocaleString());
}

const $ = (id: string): HTMLElement => {
  const el = document.getElementById(id);
  if (!el) {
    throw new Error(`element #${id} not found in index.html`);
  }
  return el;
};

// ---- Connection status dot --------------------------------------------

type ConnState = "connected" | "connecting" | "disconnected";

function setConn(state: ConnState): void {
  const el = $("conn");
  el.classList.remove(
    "conn-connected",
    "conn-connecting",
    "conn-disconnected",
  );
  el.classList.add(`conn-${state}`);
  const label =
    state === "connected"
      ? "接続中"
      : state === "disconnected"
        ? "再接続中…"
        : "接続待ち";
  const labelEl = el.querySelector(".conn-label");
  if (labelEl) labelEl.textContent = label;
}

// ---- View router ------------------------------------------------------

type ViewId = "home" | "notifications" | "health" | "jobs";
let activeView: ViewId = "home";

function showView(view: ViewId, category?: JobCategory): void {
  activeView = view;
  for (const id of ["home", "notifications", "health", "jobs"] as ViewId[]) {
    $(`view-${id}`).hidden = id !== view;
  }
  // Sidebar active state: for a jobs view, the active item is the one
  // matching the chosen category.
  document.querySelectorAll<HTMLElement>(".nav-item").forEach((b) => {
    const matches =
      b.dataset.view === view &&
      (view !== "jobs" || b.dataset.category === category);
    b.classList.toggle("active", matches);
  });
  if (view === "jobs" && category) {
    activeJobsTab = category;
    renderJobsList();
  }
  if (view === "home") renderDashboard();
}

// ---- Dashboard (home) -------------------------------------------------

let lastSnapshot: StateSnapshot | null = null;

function renderDashboard(): void {
  // Health summary card.
  const hv = $("card-health-value");
  const card = $("card-health");
  card.classList.remove("ok", "warn", "fail");
  if (!lastSnapshot) {
    hv.textContent = "読み込み中…";
  } else {
    const s = lastSnapshot;
    const fails = s.checks.filter((c) => c.status === "fail").length;
    const warns = s.checks.filter((c) => c.status === "warn").length;
    let text: string;
    let level: "ok" | "warn" | "fail";
    if (!s.online) {
      text = "❌ オフライン";
      level = "fail";
    } else if (fails > 0) {
      text = `❌ ${fails}件の異常`;
      level = "fail";
    } else if (warns > 0) {
      text = `⚠️ ${warns}件の注意`;
      level = "warn";
    } else if (s.checks.length === 0) {
      text = "✅ オンライン";
      level = "ok";
    } else if (s.checks.every((c) => c.status === "ok")) {
      text = "✅ 全て正常";
      level = "ok";
    } else {
      // Some checks couldn't run (all/partly unknown). Don't claim
      // "全て正常" — surface the unresolved count instead of a false clear.
      const unknowns = s.checks.filter((c) => c.status === "unknown").length;
      text = `❔ ${unknowns}件が判定不可`;
      level = "warn";
    }
    hv.textContent = text;
    card.classList.add(level);
  }
  updateNotifBadges();
  updateRunsCard();
}

function updateRunsCard(): void {
  const active = activeRunCount();
  const card = $("card-runs");
  card.hidden = active === 0;
  $("card-runs-value").textContent = active > 0 ? `${active}件 実行中` : "—";
}

// ---- Health view (#290) -----------------------------------------------

let healthTimer: number | undefined;

async function renderHealth() {
  const el = $("health");
  try {
    const s = await invoke<StateSnapshot>("state_snapshot");
    lastSnapshot = s;
    el.innerHTML = renderSnapshot(s);
    hydrateIcons();
    if (activeView === "home") renderDashboard();
  } catch (err) {
    // Keep the failure local (connection-state transitions are
    // event-driven via klp-disconnected, #468).
    el.innerHTML = `<p class="error">ヘルス情報を取得できません: ${escapeHtml(String(err))}</p>`;
  }
}

function renderSnapshot(s: StateSnapshot): string {
  const restartPending =
    s.target_version !== "" && s.target_version !== s.agent_version;

  // Defensive: `state.snapshot` always serializes `checks` (a Vec,
  // never null), but guard against a malformed/partial payload so a
  // missing field degrades to "no checks" instead of throwing.
  const checks = Array.isArray(s.checks) ? s.checks : [];

  // Roll the per-check results up into one headline status. Severity
  // wins: any fail → fail, else any warn → warn, else all-ok → ok.
  // Everything left — no checks yet, or checks that couldn't run — is
  // "unknown". Critically, a bucket of all-`unknown` checks must NOT
  // fall through to "ok": that would be a false all-clear.
  const fails = checks.filter((c) => c.status === "fail").length;
  const warns = checks.filter((c) => c.status === "warn").length;
  const oks = checks.filter((c) => c.status === "ok").length;
  const unknowns = checks.length - fails - warns - oks;
  const overall: CheckStatus =
    fails > 0
      ? "fail"
      : warns > 0
        ? "warn"
        : oks > 0 && oks === checks.length
          ? "ok"
          : "unknown";

  const headline =
    checks.length === 0
      ? "チェック項目はまだありません"
      : fails > 0
        ? `${fails} 件の対応が必要です`
        : warns > 0
          ? `${warns} 件の注意があります`
          : unknowns > 0
            ? `${unknowns} 件が判定できません`
            : "すべて正常です";

  // Count chips — only the non-empty buckets, most-severe first.
  const chips = [
    fails > 0 ? statusChip("fail", fails) : "",
    warns > 0 ? statusChip("warn", warns) : "",
    unknowns > 0 ? statusChip("unknown", unknowns) : "",
    oks > 0 ? statusChip("ok", oks) : "",
  ].join("");

  const restart = restartPending
    ? ` · 更新 v${escapeHtml(s.target_version)} 適用待ち（再起動）`
    : "";

  const hero = `
    <section class="health-hero status-${overall}">
      <span class="health-badge" aria-hidden="true"><i data-lucide="${STATUS_ICON[overall]}"></i></span>
      <div class="health-hero-body">
        <p class="health-hero-title">${escapeHtml(headline)}</p>
        <p class="health-hero-sub">
          <span class="health-online ${s.online ? "is-online" : "is-offline"}"><span class="health-online-dot"></span>${
            s.online ? "オンライン" : "オフライン"
          }</span>
          <span class="health-sep">·</span>Agent v${escapeHtml(s.agent_version)}${restart}
        </p>
      </div>
      ${chips ? `<div class="health-stats">${chips}</div>` : ""}
    </section>`;

  const rows = checks.length
    ? `<ul class="checks">${checks.map(renderCheck).join("")}</ul>`
    : "";
  return hero + rows;
}

// One pill in the hero's count strip, tinted by status via the
// `status-*` class (see .health-chip in CSS).
function statusChip(status: CheckStatus, n: number): string {
  return `<span class="health-chip status-${status}">${n} ${STATUS_LABEL[status]}</span>`;
}

function renderCheck(c: Check): string {
  const icon = STATUS_ICON[c.status] ?? STATUS_ICON.unknown;
  const detail = c.detail
    ? `<span class="check-detail">${escapeHtml(c.detail)}</span>`
    : "";
  const fix = c.troubleshoot
    ? `<button class="fix-btn" data-job-id="${escapeHtml(c.troubleshoot)}" data-check="${escapeHtml(c.name)}">修復する</button>`
    : "";
  // Show the operator's human title when set; fall back to the slug.
  const title = c.label && c.label.trim() ? c.label : c.name;
  const label = STATUS_LABEL[c.status] ?? STATUS_LABEL.unknown;
  return `
    <li class="check-card status-${escapeHtml(c.status)}">
      <span class="check-badge" role="img" aria-label="${escapeHtml(label)}"><i data-lucide="${icon}"></i></span>
      <div class="check-text">
        <span class="check-name">${escapeHtml(title)}</span>
        ${detail}
      </div>
      ${fix}
    </li>`;
}

// ---- Connection lifecycle ---------------------------------------------

// Pending get_handshake retry timer — tracked so a fresh klp-connected
// (or initial call) cancels an in-flight retry instead of spawning a
// second concurrent retry loop (Gemini #636).
let connectTimeout: number | undefined;

// Built-in fallback product name shown until (and when) the agent
// reports no operator-configured `client_display_name`. The Rust source
// of truth is `kanade_shared::DEFAULT_CLIENT_DISPLAY_NAME` (aliased by
// app.rs + the agent shortcut); this WebView literal + index.html's
// <title> must mirror it so an unconfigured fleet shows one name
// everywhere.
const DEFAULT_DISPLAY_NAME = "端末管理支援ツール";

// The handshake fields main.ts cares about (subset of
// kanade_shared::ipc::handshake::HandshakeResult; only the brand name
// is read here).
type Handshake = { client_display_name?: string | null };

// Brand the in-page header + the document title from the
// operator-configured product name, falling back to the built-in
// default. The OS window title is set natively in app.rs; setting
// document.title here keeps the WebView's own title in step.
function applyDisplayName(name: string | null | undefined): void {
  const display = (name ?? "").trim() || DEFAULT_DISPLAY_NAME;
  document.title = display;
  const el = document.querySelector(".brand-name");
  if (el) el.textContent = display;
}

// Called on each (re)connect: confirm the pipe is up (get_handshake
// errors when it isn't), flip the status dot, and (re)pull all data.
async function onConnected() {
  if (connectTimeout !== undefined) {
    clearTimeout(connectTimeout);
    connectTimeout = undefined;
  }
  try {
    // Doubles as the pipe-readiness probe (it errors when the agent
    // isn't up yet) and the source of the operator-configured brand.
    const hs = await invoke<Handshake>("get_handshake");
    applyDisplayName(hs?.client_display_name ?? null);
  } catch {
    // Pipe not ready yet — the supervisor retries; show "connecting".
    setConn("connecting");
    connectTimeout = window.setTimeout(onConnected, 2000);
    return;
  }
  setConn("connected");
  void renderHealth();
  if (healthTimer === undefined) {
    healthTimer = window.setInterval(renderHealth, 10000);
  }
  void loadJobs();
  void loadNotifications();
}

// Escape the five characters that actually matter in HTML text /
// double-quoted-attribute contexts. We deliberately do NOT encode `/`
// (the legacy OWASP `&#x2F;` is only needed for JSON-inside-`<script>`,
// not here) — leaving it raw keeps element `id`s equal to their source
// value (so `getElementById` matches without a decode round-trip) and
// keeps slugs/paths readable in the DOM (Claude #636).
function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

window.addEventListener("DOMContentLoaded", () => {
  // Hydrate the static sidebar / dashboard icons once.
  hydrateIcons();

  // Show this client binary's version dim at the sidebar foot (mirrors the
  // SPA's /api/version badge). Baked at compile time, so it reflects the
  // *running* process — not whatever a fleet-deploy just swapped onto disk.
  void invoke<string>("app_version")
    .then((v) => {
      const el = document.getElementById("app-version");
      if (el) {
        el.textContent = `v${v}`;
        // Reveal only once filled. Starting `hidden` (vs a CSS `:empty`
        // rule) survives an HTML formatter inserting whitespace into the
        // element, which would otherwise defeat `:empty`.
        el.hidden = false;
      }
    })
    .catch((e) => console.error("app_version failed", e));

  // Delegated click handling: a single document-level listener survives
  // the innerHTML churn that per-element listeners wouldn't.
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;

    // Nav item / dashboard card → switch view.
    const nav = t.closest<HTMLElement>(".nav-item, .dash-card");
    if (nav?.dataset.view) {
      const v = nav.dataset.view as ViewId;
      showView(v, nav.dataset.category as JobCategory | undefined);
      return;
    }

    // Health remediation: run the check's `troubleshoot` job.
    const fixBtn = t.closest<HTMLButtonElement>(".fix-btn");
    if (fixBtn?.dataset.jobId) {
      fixBtn.disabled = true;
      void executeJob(
        fixBtn.dataset.jobId,
        fixBtn.dataset.check ?? fixBtn.dataset.jobId,
      );
      return;
    }
    const killBtn = t.closest<HTMLElement>(".kill-btn");
    if (killBtn?.dataset.runId) {
      void killRun(killBtn.dataset.runId);
      return;
    }
    // Run header clicked → expand/collapse a completed run's output.
    // Checked after the kill button so 中止 (only on running rows, which
    // aren't collapsible anyway) always wins its own click.
    const runToggle = t.closest<HTMLElement>("[data-run-toggle]");
    if (runToggle?.dataset.runToggle) {
      toggleRun(runToggle.dataset.runToggle);
      return;
    }
    // Notification title clicked → expand/collapse its body (and mark an
    // ack-optional one read on open). Checked before the ack button, but
    // they live in separate subtrees so order is not load-bearing.
    const toggle = t.closest<HTMLElement>("[data-notif-toggle]");
    if (toggle?.dataset.notifToggle) {
      toggleNotification(toggle.dataset.notifToggle);
      return;
    }
    // Notification ack — the explicit 確認 button on ack-required rows.
    const ackBtn = t.closest<HTMLButtonElement>(".notif-ack-btn");
    if (ackBtn?.dataset.notifId) {
      ackBtn.disabled = true;
      ackNotification(ackBtn.dataset.notifId).catch(() => {
        ackBtn.disabled = false;
      });
      return;
    }
    // Job run button.
    const runBtn = t.closest<HTMLButtonElement>(".job-run-btn");
    if (runBtn?.dataset.jobId) {
      runBtn.disabled = true;
      void executeJob(
        runBtn.dataset.jobId,
        runBtn.dataset.label ?? runBtn.dataset.jobId,
      ).finally(() => {
        runBtn.disabled = false;
      });
      return;
    }
  });

  // Keyboard activation for the notification toggle (it carries
  // role="button" + tabindex="0", so Enter/Space must work like a click).
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Enter" && e.key !== " ") return;
    // `e.target` can be the Document (no focused element) — guard before
    // `closest`, which only exists on Element.
    if (!(e.target instanceof Element)) return;
    const toggle = e.target.closest<HTMLElement>("[data-notif-toggle]");
    if (toggle?.dataset.notifToggle) {
      // A native <button> toggle (the "開いて確認" control) already emits its
      // own click on Enter/Space — synthesizing one here too would toggle
      // twice (net no-op). Only the role="button" head needs this shim.
      if (toggle.tagName === "BUTTON") return;
      e.preventDefault();
      toggleNotification(toggle.dataset.notifToggle);
      return;
    }
    // Same keyboard affordance for the run-output disclosure header.
    const runToggle = e.target.closest<HTMLElement>("[data-run-toggle]");
    if (runToggle?.dataset.runToggle) {
      e.preventDefault();
      toggleRun(runToggle.dataset.runToggle);
    }
  });

  // Agent→client pushes forwarded as `klp-notification` events (#467).
  void listen<RpcNotification>("klp-notification", (event) => {
    const payload = event.payload as Partial<RpcNotification> | null;
    if (typeof payload?.method !== "string") return;
    if (payload.method === "jobs.progress") {
      const p = payload.params as Partial<JobProgress> | null;
      if (!p?.run_id) return;
      handleProgress(p as JobProgress);
      return;
    }
    if (payload.method === "notifications.new") {
      const n = payload.params as Partial<AppNotification> | null;
      if (!n?.id) return;
      handleNewNotification(n as AppNotification);
      return;
    }
    if (payload.method === "notifications.amended") {
      const a = payload.params as Partial<NotificationAmend> | null;
      if (!a?.id || !a.op) return;
      handleAmendNotification(a as NotificationAmend);
      return;
    }
  });

  // Reconnect lifecycle (#468).
  void listen("klp-connected", () => {
    jobsLoaded = false;
    notifSubscribed = false;
    void onConnected();
  });
  void listen("klp-disconnected", () => {
    if (healthTimer !== undefined) {
      window.clearInterval(healthTimer);
      healthTimer = undefined;
    }
    setConn("disconnected");
  });

  // Single-instance forward (#624): a second launch carried an emergency
  // id; the running instance toasts it here instead of a new process.
  void listen<string>("klp-show-notification", (event) => {
    const id = event.payload;
    if (id) void surfaceForwardedToast(id);
  });

  // Presence-driven re-surface (#647): a second `--resurface` launch was
  // collapsed into this instance; re-pop every unread toast notification.
  void listen("klp-resurface", () => {
    void resurfaceAllToasts();
  });

  // Toast clicked (#647): a `kanade-client://show?id=<id>` launch was forwarded
  // to this running instance (the window was already revealed from Rust) —
  // scroll to + flash the notification.
  void listen<string>("klp-focus-notification", (event) => {
    const id = event.payload;
    if (id) focusNotificationInPanel(id);
  });
  window.setInterval(checkStuckRuns, 60_000);
  window.setInterval(sweepExpired, 60_000);

  // Initial connect attempt (the supervisor may already be connected
  // before this WebView loaded; klp-connected also drives reconnects).
  void onConnected();
});
