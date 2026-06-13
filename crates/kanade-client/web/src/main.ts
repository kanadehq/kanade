// Kanade Client App WebView entry point.
//
// Sprint 8 skeleton: on load, ask the Tauri backend for the
// cached handshake result, render "Connected to agent vX.Y.Z as
// DOMAIN\\user (pc_id)". Wire up a "Ping" button that
// round-trips system.ping through the backend's invoke handler
// and shows the agent's wall-clock.
//
// The Health tab (#290) renders the agent's state.snapshot below;
// each check's 「修復する」 button runs its `troubleshoot` job via
// `jobs.execute` (#291). A job catalog (アップデート / 困ったとき /
// カタログ tabs via `jobs.list`) lets the user run any user-invokable
// job; a live run section tracks every execute from `jobs.progress`
// pushes (forwarded as `klp-notification` events, #467). The page is
// intentionally one-screen and dependency-light (no framework) so a
// later UI redesign isn't fighting any priors.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  onAction,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

type HandshakeSession = {
  user: string;
  session_id: number;
  pc_id: string;
};

type HandshakeResult = {
  protocol: number;
  agent_version: string;
  features: string[];
  session: HandshakeSession;
};

type PingResult = {
  agent_time: string;
};

// Mirrors `kanade_shared::ipc::state` (#290). Hand-written for the
// same reason the Handshake/Ping types are — the client doesn't
// generate TS bindings yet; a future PR can wire schemars → ts.
type CheckStatus = "ok" | "warn" | "fail" | "unknown";

type Check = {
  name: string;
  status: CheckStatus;
  detail?: string | null;
  troubleshoot?: string | null;
};

type StateSnapshot = {
  pc_id: string;
  online: boolean;
  vpn: string;
  checks: Check[];
  agent_version: string;
  target_version: string;
};

const STATUS_ICON: Record<CheckStatus, string> = {
  ok: "✅",
  warn: "⚠️",
  fail: "❌",
  unknown: "❔",
};

// ---- Jobs / remediation (#291) ----

// Mirrors `kanade_shared::ipc::jobs` — hand-written like the other
// IPC types until TS bindings are generated.
type RunStatus = "queued" | "running" | "completed" | "failed" | "killed";

type JobProgress = {
  run_id: string;
  status: RunStatus;
  stdout_chunk?: string | null;
  stderr_chunk?: string | null;
  exit_code?: number | null;
};

type JobsExecuteResult = { run_id: string };

type JobCategory = "software_update" | "troubleshoot" | "catalog";

type UserInvokableJob = {
  id: string;
  display_name: string;
  display_description?: string | null;
  icon?: string | null;
  category: JobCategory;
  version: string;
  // `last_run` is in the wire shape but unused by the catalog view yet.
};

type JobsListResult = { items: UserInvokableJob[] };

// The raw `RpcNotification` the backend re-emits as a `klp-notification`
// Tauri event; we switch on `method`.
type RpcNotification = { jsonrpc: string; method: string; params: unknown };

// The three Client App job tabs (SPEC §2.1), in display order. The
// `category` matches `kanade_shared::ipc::jobs::JobCategory`.
const CATEGORY_TABS: { category: JobCategory; label: string }[] = [
  { category: "software_update", label: "アップデート" },
  { category: "troubleshoot", label: "困ったとき" },
  { category: "catalog", label: "カタログ" },
];

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
  // The check name (or job id) — a human label for the run row.
  label: string;
  status: RunStatus;
  // Accumulated stdout/stderr tail shown under the row.
  output: string;
  // Date.now() of creation or last progress. Drives the stuck-run
  // watchdog (a non-terminal run with no progress for too long).
  updatedAt: number;
};

// Active + recently-finished runs, keyed by run_id. Insertion order
// drives the render (newest shown first). Bounded by MAX_RUNS so a
// long-lived WebView (auto-launched, left open for days) doesn't
// accumulate stale terminal rows forever.
const runs = new Map<string, Run>();
const MAX_RUNS = 30;

function isTerminal(status: RunStatus): boolean {
  return (
    status === "completed" || status === "failed" || status === "killed"
  );
}

// Evict the oldest *terminal* runs once over the cap. In-flight runs
// (queued / running) are never evicted — they still have progress to
// receive. Insertion order means the iterator yields oldest first.
function evictOldRuns(): void {
  if (runs.size <= MAX_RUNS) return;
  for (const [id, r] of runs) {
    if (runs.size <= MAX_RUNS) break;
    if (isTerminal(r.status)) runs.delete(id);
  }
}

// Stuck-run watchdog. `jobs.execute` streams no intermediate progress
// between the Running push and the terminal one (#465), so the client
// can't tell a long-running job from a dead agent by silence alone. Use
// a deadline safely above any realistic user-invokable job (the winget
// examples cap at ~10 min) and only THEN flag a still-non-terminal run
// as unresponsive — otherwise a legitimately slow install would be
// false-flagged. A finer signal needs agent-side heartbeat / incremental
// progress (#469).
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

// Execute a remediation job (the check's `troubleshoot` id) and track
// its run; progress arrives asynchronously via `klp-notification`.
async function executeJob(jobId: string, label: string): Promise<void> {
  try {
    const r = await invoke<JobsExecuteResult>("jobs_execute", { id: jobId });
    // Race: a `jobs.progress` push for this run can arrive (via the
    // notification listener) BEFORE this invoke promise resolves. If it
    // did, `handleProgress` already created the row — keep its
    // status/output and just attach the human label, don't clobber it
    // back to "running" with empty output.
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
  } catch (err) {
    // The execute call itself was rejected (e.g. Unauthorized / not
    // found) — surface it as a synthetic failed row so the user sees
    // why, instead of a click that silently does nothing.
    const pseudoId = `error-${jobId}-${runs.size}`;
    runs.set(pseudoId, {
      runId: pseudoId,
      label,
      status: "failed",
      output: `実行できませんでした: ${String(err)}`,
      updatedAt: Date.now(),
    });
    renderRuns();
  }
}

// Best-effort kill of a running job. The terminal `jobs.progress`
// (status = killed) still arrives and updates the row. If the RPC
// itself fails (e.g. the run already finished), surface a note on the
// row so the lingering 中止 button doesn't look broken.
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

// Apply one `jobs.progress` push to the matching run row.
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
  if (p.stdout_chunk) run.output += p.stdout_chunk;
  if (p.stderr_chunk) run.output += p.stderr_chunk;
  runs.set(p.run_id, run);
  renderRuns();
}

function renderRuns(): void {
  evictOldRuns();
  const section = $("runs-section");
  if (runs.size === 0) {
    section.hidden = true;
    return;
  }
  section.hidden = false;
  // Newest first.
  const list = [...runs.values()].reverse();
  const container = $("runs");
  // Structural change (a run added / evicted) → full render. Otherwise
  // update each row IN PLACE so a status/output tick doesn't blow away
  // the user's scroll position or text selection in another run's
  // output (progress can tick several times a second for a chatty job).
  if (container.children.length !== list.length) {
    container.innerHTML = list.map(renderRun).join("");
    return;
  }
  for (const r of list) {
    const el = document.getElementById(`run-${r.runId}`);
    if (!el) continue;
    const tmp = document.createElement("div");
    tmp.innerHTML = renderRun(r);
    const fresh = tmp.firstElementChild;
    if (fresh) el.replaceWith(fresh);
  }
}

function renderRun(r: Run): string {
  const icon = RUN_STATUS_ICON[r.status] ?? "⏳";
  const label = RUN_STATUS_LABEL[r.status] ?? r.status;
  const kill =
    r.status === "running" || r.status === "queued"
      ? `<button class="kill-btn" data-run-id="${escapeHtml(r.runId)}">中止</button>`
      : "";
  // Cap the shown output so a chatty job can't blow up the DOM; the
  // tail is what matters for "did it work".
  const output = r.output.trim()
    ? `<pre class="run-output">${escapeHtml(r.output.slice(-4000))}</pre>`
    : "";
  // `id` lets renderRuns update this row in place (see above). runIds
  // are agent-minted UUIDs / internal slugs, so escapeHtml is a no-op
  // here, but keep it escaped for the same XSS-hygiene reason as below.
  return `
    <div class="run-row status-${escapeHtml(r.status)}" id="run-${escapeHtml(r.runId)}">
      <span class="run-icon">${icon}</span>
      <span class="run-name">${escapeHtml(r.label)}</span>
      <span class="run-status muted">${escapeHtml(label)}</span>
      ${kill}
      ${output}
    </div>`;
}

// ---- Job catalog (#291): the three user-invokable job tabs ----

// Jobs grouped by category, loaded once on connect via `jobs.list`.
const jobsByCategory = new Map<JobCategory, UserInvokableJob[]>();
let activeJobsTab: JobCategory = CATEGORY_TABS[0].category;
// Re-entry guard: loadJobs is fired once per connect today, but
// reconnect (#468) will call it again — the flag stops two overlapping
// loads racing the `clear()` + refill against a tab-click read. Reset
// on failure so the next connect retries.
let jobsLoaded = false;

// Fetch the user-invokable job catalog and render the tabs. Called
// once the agent is connected. The catalog changes rarely, so this is
// a one-shot load (not polled) — re-run on reconnect when that lands.
async function loadJobs(): Promise<void> {
  if (jobsLoaded) return;
  jobsLoaded = true;
  const section = $("jobs-section");
  try {
    // `category: null` → the agent returns every tab's jobs; we group
    // client-side so a single round-trip fills all three tabs.
    const res = await invoke<JobsListResult>("jobs_list", { category: null });
    jobsByCategory.clear();
    for (const job of res.items) {
      const list = jobsByCategory.get(job.category) ?? [];
      list.push(job);
      jobsByCategory.set(job.category, list);
    }
    if (res.items.length === 0) {
      // No user-invokable jobs registered → nothing to show.
      section.hidden = true;
      return;
    }
    section.hidden = false;
    // If the default tab is empty, jump to the first tab that has jobs.
    if ((jobsByCategory.get(activeJobsTab)?.length ?? 0) === 0) {
      const firstNonEmpty = CATEGORY_TABS.find(
        (t) => (jobsByCategory.get(t.category)?.length ?? 0) > 0,
      );
      if (firstNonEmpty) activeJobsTab = firstNonEmpty.category;
    }
    renderJobsTabs();
    renderJobsList();
  } catch (err) {
    // Let a later (re)connect retry, and don't show a bare empty tab
    // bar next to the error.
    jobsLoaded = false;
    section.hidden = false;
    $("jobs-tabs").hidden = true;
    $("jobs-list").innerHTML =
      `<p class="error">ジョブ一覧を取得できません: ${escapeHtml(String(err))}</p>`;
  }
}

function renderJobsTabs(): void {
  $("jobs-tabs").hidden = false;
  $("jobs-tabs").innerHTML = CATEGORY_TABS.map((t) => {
    const count = jobsByCategory.get(t.category)?.length ?? 0;
    const active = t.category === activeJobsTab ? " active" : "";
    return `<button class="jobs-tab${active}" data-category="${t.category}">${escapeHtml(t.label)} (${count})</button>`;
  }).join("");
}

function renderJobsList(): void {
  const jobs = jobsByCategory.get(activeJobsTab) ?? [];
  if (jobs.length === 0) {
    $("jobs-list").innerHTML =
      `<p class="muted">このカテゴリのジョブはありません</p>`;
    return;
  }
  $("jobs-list").innerHTML = jobs.map(renderJobRow).join("");
}

function renderJobRow(j: UserInvokableJob): string {
  const desc = j.display_description
    ? `<span class="job-desc">${escapeHtml(j.display_description)}</span>`
    : "";
  // Button BEFORE the description: `.job-desc` is `flex: 1 1 100%` (it
  // wraps to its own row), so the button must precede it to sit on the
  // name's row (pushed right via margin-left:auto) rather than being
  // bumped to a third line.
  return `
    <div class="job-row">
      <span class="job-name">${escapeHtml(j.display_name)}</span>
      <button class="job-run-btn" data-job-id="${escapeHtml(j.id)}" data-label="${escapeHtml(j.display_name)}">実行</button>
      ${desc}
    </div>`;
}

// ---- Notifications (Phase E, #102) ----

// Mirrors `kanade_shared::ipc::notifications` — hand-written like the
// other IPC types until TS bindings are generated. `unknown` is the
// serde forward-compat catch-all (#492): a newer agent's new priority
// decodes here and we render it neutrally rather than throwing.
type NotificationPriority = "info" | "warn" | "emergency" | "unknown";

type AppNotification = {
  id: string;
  priority: NotificationPriority;
  require_ack: boolean;
  title: string;
  body: string;
  issued_at: string;
  issued_by?: string | null;
  expires_at?: string | null;
  // `acked_at` from THIS user's perspective; populated by
  // notifications.list for already-acked entries, set locally on ack.
  acked_at?: string | null;
};

type NotificationsListResult = {
  items: AppNotification[];
  next_cursor?: string | null;
};

type NotificationsAckResult = { acked_at: string };

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

// All known notifications, keyed by id, in insertion (≈ arrival) order.
// Newest is rendered first. Acked + unread both kept so the panel
// doubles as recent history; expired ones are filtered at render time.
const notifications = new Map<string, AppNotification>();

// Guards a double subscribe/list on the same connection (renderStatus
// can be called more than once per connect). Reset on reconnect so a
// fresh connection re-subscribes — the agent's subscription is
// per-connection, so a stale flag would leave the new pipe push-less.
let notifSubscribed = false;

function isExpired(n: AppNotification): boolean {
  if (!n.expires_at) return false;
  const t = Date.parse(n.expires_at);
  return !Number.isNaN(t) && t <= Date.now();
}

// Subscribe to live pushes and load recent history. Called on every
// (re)connect; the guard makes a redundant same-connection call a no-op.
async function loadNotifications(): Promise<void> {
  // Subscribe once per connection (the guard makes a redundant
  // same-connection call a no-op); the agent's subscription is
  // per-connection so a failure here leaves the flag false to retry.
  if (!notifSubscribed) {
    try {
      await invoke("notifications_subscribe");
      notifSubscribed = true;
    } catch (err) {
      console.error("notifications_subscribe failed", err);
      return;
    }
  }
  // Load history on every call (not gated by `notifSubscribed`): a
  // transient list failure must not wedge the panel empty for the rest
  // of the connection — the next (re)connect retries it, and the
  // re-set of map entries is idempotent.
  try {
    // Load `all` (acked + unread) so the panel shows recent history,
    // with the unread badge derived from `acked_at`. The agent clamps
    // the limit; one page is plenty for a glanceable panel.
    const res = await invoke<NotificationsListResult>("notifications_list", {
      filter: "all",
      cursor: null,
    });
    for (const n of res.items) notifications.set(n.id, n);
    renderNotifications();

    await ensureLaunchId();
    if (pendingEmergencyId) {
      // Agent-launched for a specific emergency (#102): surface it as a
      // native toast (the window is hidden so it never bursts over a
      // meeting) and DON'T pop the blocking modal — clicking the toast
      // opens the window onto this notification instead.
      const target = notifications.get(pendingEmergencyId);
      if (target && !target.acked_at && !isExpired(target)) {
        void surfaceEmergencyToast(target);
      } else {
        // Already acked / expired / unknown — nothing to surface; reveal
        // the window so the launch isn't a silent no-op.
        void invoke("show_main_window").catch(() => {});
      }
    } else {
      // Normal launch — recovery path (SPEC §2.12.8): an unacked,
      // non-expired emergency missed while disconnected re-raises its
      // modal from history, not just a push.
      for (const n of res.items) {
        if (n.priority === "emergency" && !n.acked_at && !isExpired(n)) {
          showEmergencyModal(n);
        }
      }
    }
  } catch (err) {
    console.error("notifications_list failed", err);
  }
}

// ---- Emergency launch (#102): agent → `--show-notification <id>` ----

// The notification id the agent launched us to surface, or null on a
// normal launch. Fetched once (lazily) from the Rust side.
let pendingEmergencyId: string | null = null;
let launchIdFetched = false;
// One-shot guard so a reconnect's loadNotifications doesn't re-toast.
let emergencyToasted = false;
// `onAction` is registered at most once for the app's lifetime.
let emergencyActionRegistered = false;

async function ensureLaunchId(): Promise<void> {
  if (launchIdFetched) return;
  launchIdFetched = true;
  try {
    pendingEmergencyId =
      (await invoke<string | null>("get_launch_notification")) ?? null;
  } catch (err) {
    console.error("get_launch_notification failed", err);
    pendingEmergencyId = null;
  }
}

// Show the emergency as a native OS toast (hidden window). Clicking it
// reveals the window focused on the notification panel.
async function surfaceEmergencyToast(n: AppNotification): Promise<void> {
  if (emergencyToasted) return;
  emergencyToasted = true;
  try {
    let granted = await isPermissionGranted();
    if (!granted) granted = (await requestPermission()) === "granted";
    if (!granted) {
      // No toast permission — don't leave the emergency invisible; reveal
      // the window and focus the notification directly.
      await invoke("show_main_window").catch(() => {});
      focusNotificationInPanel(n.id);
      return;
    }
    if (!emergencyActionRegistered) {
      emergencyActionRegistered = true;
      // Fires when the user clicks any OS notification this app sent.
      // Scope assumption: `sendNotification` (the OS-toast path) is ONLY
      // called from here for the emergency, so every onAction is an
      // emergency-toast click. In-app DOM toasts (showToast) don't fire
      // onAction. If a future change sends OS notifications elsewhere,
      // this handler would need to disambiguate by notification id.
      await onAction(() => {
        void openEmergencyFromToast();
      });
    }
    sendNotification({ title: `🚨 ${n.title}`, body: n.body });
  } catch (err) {
    console.error("emergency toast failed; revealing window instead", err);
    await invoke("show_main_window").catch(() => {});
    focusNotificationInPanel(n.id);
  }
}

// Toast clicked → reveal + focus the window, scroll to the emergency.
async function openEmergencyFromToast(): Promise<void> {
  await invoke("show_main_window").catch(() => {});
  if (pendingEmergencyId) focusNotificationInPanel(pendingEmergencyId);
}

// Scroll the notification panel to a notification and briefly flash it.
function focusNotificationInPanel(id: string): void {
  renderNotifications();
  const el = document.getElementById(`cnotif-${id}`);
  if (!el) return;
  el.scrollIntoView({ behavior: "smooth", block: "center" });
  el.classList.add("notif-flash");
  window.setTimeout(() => el.classList.remove("notif-flash"), 2000);
}

// Apply one `notifications.new` push: store, re-render, and surface it
// (emergency → blocking modal; info/warn → transient toast).
function handleNewNotification(n: AppNotification): void {
  notifications.set(n.id, n);
  renderNotifications();
  if (isExpired(n)) return;
  if (n.priority === "emergency") {
    showEmergencyModal(n);
  } else {
    showToast(n);
  }
}

// Ack a notification for this OS user. Marks it read locally on success
// and dismisses its emergency modal if one is open.
async function ackNotification(id: string): Promise<void> {
  try {
    const r = await invoke<NotificationsAckResult>("notifications_ack", { id });
    const n = notifications.get(id);
    if (n) n.acked_at = r.acked_at;
    dismissEmergencyModal(id);
    renderNotifications();
  } catch (err) {
    console.error("notifications_ack failed", err);
    // Surface on the open modal (the user is staring at it) so a failed
    // ack doesn't look like an unresponsive button.
    const errEl = document.getElementById("emergency-error");
    if (errEl) errEl.textContent = `確認に失敗しました: ${String(err)}`;
    // Re-throw so the click handler can re-enable its disabled button
    // and let the user retry (a transient failure shouldn't wedge it).
    throw err;
  }
}

function renderNotifications(): void {
  evictOldNotifications();
  const section = $("notifications-section");
  // Sort newest-first by issued_at rather than leaning on Map insertion
  // order: history (notifications.list) arrives newest-first but live
  // pushes append to the end, so insertion order is a scramble — an
  // explicit sort is the only correct ordering.
  const live = [...notifications.values()]
    .filter((n) => !isExpired(n))
    .sort((a, b) => Date.parse(b.issued_at) - Date.parse(a.issued_at));
  if (live.length === 0) {
    section.hidden = true;
    return;
  }
  section.hidden = false;
  const unread = live.filter((n) => !n.acked_at).length;
  const badge = $("notif-badge");
  if (unread > 0) {
    badge.hidden = false;
    badge.textContent = String(unread);
  } else {
    badge.hidden = true;
  }
  $("notifications").innerHTML = live.map(renderNotification).join("");
}

// Cap the notifications map so a long-lived client fed a high volume of
// non-expiring notifications doesn't grow it unbounded (mirrors the
// `runs` MAX_RUNS eviction). Drop the oldest ACKED entries first —
// unread ones still need surfacing — and only once over the cap.
const MAX_NOTIFICATIONS = 100;

function evictOldNotifications(): void {
  if (notifications.size <= MAX_NOTIFICATIONS) return;
  // Insertion order ≈ oldest-first for history; acked entries are the
  // safe ones to forget (already confirmed, dropped from the unread
  // badge). Iterate oldest-first and evict acked until back under cap.
  for (const [id, n] of notifications) {
    if (notifications.size <= MAX_NOTIFICATIONS) break;
    if (n.acked_at) notifications.delete(id);
  }
}

function renderNotification(n: AppNotification): string {
  const icon = PRIORITY_ICON[n.priority] ?? PRIORITY_ICON.unknown;
  const acked = !!n.acked_at;
  const meta = [
    n.issued_by ? `送信元: ${escapeHtml(n.issued_by)}` : "",
    fmtTime(n.issued_at),
  ]
    .filter(Boolean)
    .join(" · ");
  // Ack control: a 確認 button only while require_ack and not yet acked;
  // once acked (any notification), a quiet "確認済み" marker.
  const ackCtl =
    n.require_ack && !acked
      ? `<button class="notif-ack-btn" data-notif-id="${escapeHtml(n.id)}">確認</button>`
      : acked
        ? `<span class="notif-acked muted">✓ 確認済み</span>`
        : "";
  // `id` lets the emergency-launch flow scroll to + flash this row
  // (focusNotificationInPanel). n.id is an agent-minted UUID so escapeHtml
  // is a no-op, but keep it escaped for the same XSS-hygiene reason.
  return `
    <div id="cnotif-${escapeHtml(n.id)}" class="notif-row priority-${escapeHtml(n.priority)}${acked ? " acked" : ""}">
      <span class="notif-icon">${icon}</span>
      <div class="notif-main">
        <div class="notif-head">
          <span class="notif-title">${escapeHtml(n.title)}</span>
          <span class="notif-prio muted">${escapeHtml(PRIORITY_LABEL[n.priority] ?? PRIORITY_LABEL.unknown)}</span>
        </div>
        <p class="notif-text">${escapeHtml(n.body)}</p>
        <p class="notif-meta muted">${meta}</p>
      </div>
      ${ackCtl}
    </div>`;
}

// Transient toast for info/warn pushes (non-blocking). Auto-dismisses;
// the notification stays in the panel for later reference / ack.
function showToast(n: AppNotification): void {
  const container = $("toast-container");
  const el = document.createElement("div");
  el.className = `toast priority-${n.priority}`;
  el.innerHTML = `
    <span class="toast-icon">${PRIORITY_ICON[n.priority] ?? PRIORITY_ICON.unknown}</span>
    <div class="toast-main">
      <strong class="toast-title">${escapeHtml(n.title)}</strong>
      <span class="toast-text">${escapeHtml(n.body)}</span>
    </div>`;
  container.appendChild(el);
  window.setTimeout(() => {
    el.classList.add("toast-out");
    window.setTimeout(() => el.remove(), 300);
  }, 6000);
}

// Emergency notifications block on a focus-grabbing modal. Only one is
// shown at a time; others queue (deduped by id) and surface as each is
// dismissed, so a burst can't stack overlapping modals.
const emergencyQueue: AppNotification[] = [];
let emergencyShown: string | null = null;

function showEmergencyModal(n: AppNotification): void {
  if (emergencyShown === n.id || emergencyQueue.some((q) => q.id === n.id)) {
    return;
  }
  emergencyQueue.push(n);
  pumpEmergency();
}

function pumpEmergency(): void {
  if (emergencyShown) return;
  const modal = $("emergency-modal");
  const next = emergencyQueue.shift();
  if (!next) {
    modal.hidden = true;
    modal.innerHTML = "";
    return;
  }
  emergencyShown = next.id;
  modal.hidden = false;
  const meta = [
    next.issued_by ? `送信元: ${escapeHtml(next.issued_by)}` : "",
    fmtTime(next.issued_at),
  ]
    .filter(Boolean)
    .join(" · ");
  // require_ack → the only way out is 確認 (which acks). Otherwise a
  // plain 閉じる that dismisses locally without acking (the operator
  // didn't ask for a confirmation).
  const btn = next.require_ack
    ? `<button class="emergency-ack-btn" data-notif-id="${escapeHtml(next.id)}">確認</button>`
    : `<button class="emergency-close-btn" data-notif-id="${escapeHtml(next.id)}">閉じる</button>`;
  modal.innerHTML = `
    <div class="emergency-card" role="alertdialog" aria-modal="true">
      <div class="emergency-head">🚨 ${escapeHtml(next.title)}</div>
      <p class="emergency-text">${escapeHtml(next.body)}</p>
      <p class="emergency-meta muted">${meta}</p>
      <p id="emergency-error" class="error"></p>
      ${btn}
    </div>`;
  // `role="alertdialog"` / `aria-modal` don't grab focus on their own,
  // so move keyboard focus onto the action button — otherwise the
  // "blocking" modal stays navigable from the background page and is
  // easy for keyboard / screen-reader users to miss.
  modal
    .querySelector<HTMLElement>(".emergency-ack-btn, .emergency-close-btn")
    ?.focus();
}

// Remove a notification from the emergency flow — whether it was queued
// or the one on screen. Advancing pulls the next queued emergency up.
function dismissEmergencyModal(id: string): void {
  const qi = emergencyQueue.findIndex((q) => q.id === id);
  if (qi >= 0) emergencyQueue.splice(qi, 1);
  if (emergencyShown === id) {
    emergencyShown = null;
    pumpEmergency();
  }
}

// Drop expired notifications from the panel and close an expired modal.
// Time-driven, so run on a timer (a notification can expire while the
// app sits idle with nothing pushing a re-render).
function sweepExpired(): void {
  if (emergencyShown) {
    const n = notifications.get(emergencyShown);
    if (n && isExpired(n)) dismissEmergencyModal(emergencyShown);
  }
  renderNotifications();
}

// Format an ISO instant as the user's local wall-clock; fall back to
// the raw string if it doesn't parse. The result is HTML-escaped at the
// source: every caller splices it into `innerHTML`, and the fallback
// branch returns an un-sanitised agent-supplied string — escaping here
// (one place) closes that DOM-XSS hole without each call site having to
// remember to wrap it (defence-in-depth; the agent pipe is high-trust).
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

async function renderStatus() {
  const status = $("status");
  try {
    const hs = await invoke<HandshakeResult>("get_handshake");
    status.innerHTML = `
      <p>Connected to agent <strong>v${escapeHtml(hs.agent_version)}</strong></p>
      <p class="muted">
        Session: ${escapeHtml(hs.session.user)} (session ${hs.session.session_id})<br />
        PC: ${escapeHtml(hs.session.pc_id)}<br />
        Protocol: v${hs.protocol}${hs.features.length ? ` — features: ${hs.features.map(escapeHtml).join(", ")}` : ""}
      </p>
    `;
    $("ping-section").hidden = false;
    // Connected → reveal the Health tab, render it now, and keep it
    // live with a light poll (the agent re-evaluates every ~30 s).
    $("health-section").hidden = false;
    void renderHealth();
    if (healthTimer === undefined) {
      healthTimer = window.setInterval(renderHealth, 10000);
    }
    // Load the user-invokable job catalog (アップデート / 困ったとき /
    // カタログ). One-shot — the catalog changes rarely.
    void loadJobs();
    // Subscribe to live notifications + load recent history (Phase E).
    void loadNotifications();
  } catch (err) {
    status.innerHTML = `<p class="error">Agent unavailable: ${escapeHtml(String(err))}</p>
      <p class="muted">Retrying in 5 s…</p>`;
    // Crude retry loop; a proper PR adds a tauri event the backend
    // emits once the pipe lands.
    setTimeout(renderStatus, 5000);
  }
}

// Health tab (#290): SPEC §2.1.5 use case 2 — render the agent's
// `state.snapshot` as a list of compliance checks (status light +
// name + detail) plus the online / VPN / version header. The agent
// re-evaluates every ~30 s, so a light poll keeps the tab live until
// `state.changed` push lands in a follow-up.
let healthTimer: number | undefined;

async function renderHealth() {
  const el = $("health");
  try {
    const s = await invoke<StateSnapshot>("state_snapshot");
    el.innerHTML = renderSnapshot(s);
  } catch (err) {
    // Show the error only in the health section; do not touch the top
    // status banner or stop the poll here. `klp-disconnected` now owns
    // both (it fires once when the supervisor's reader task exits, #468).
    // Duplicating that here is a race: a stale in-flight `state_snapshot`
    // can reject *after* `klp-connected` has already restored the
    // "Connected" banner and restarted the timer — clobbering the banner
    // back to "reconnecting…" and killing the just-restarted poll. So a
    // transient per-request failure stays local; connection-state
    // transitions are event-driven (#468).
    el.innerHTML = `<p class="error">ヘルス情報を取得できません: ${escapeHtml(String(err))}</p>`;
  }
}

function renderSnapshot(s: StateSnapshot): string {
  const restartPending =
    s.target_version !== "" && s.target_version !== s.agent_version;
  const header = `
    <p>${s.online ? "✅ オンライン" : "❌ オフライン"} · VPN: ${escapeHtml(s.vpn)}</p>
    <p class="muted">Agent v${escapeHtml(s.agent_version)}${
      restartPending
        ? ` → v${escapeHtml(s.target_version)}（再起動待ち）`
        : ""
    }</p>`;
  const rows = s.checks.length
    ? `<ul class="checks">${s.checks.map(renderCheck).join("")}</ul>`
    : `<p class="muted">チェック項目はまだありません</p>`;
  return header + rows;
}

function renderCheck(c: Check): string {
  const icon = STATUS_ICON[c.status] ?? STATUS_ICON.unknown;
  const detail = c.detail
    ? `<span class="check-detail">${escapeHtml(c.detail)}</span>`
    : "";
  // Remediation (#291): the button runs the check's `troubleshoot`
  // job via `jobs.execute`. Clicks are caught by the delegated handler
  // (the Health list is re-rendered on a poll, so per-button listeners
  // wouldn't survive). The job id + a human label ride on data-attrs.
  const fix = c.troubleshoot
    ? `<button class="fix-btn" data-job-id="${escapeHtml(c.troubleshoot)}" data-check="${escapeHtml(c.name)}">修復する</button>`
    : "";
  return `
    <li class="check-row status-${escapeHtml(c.status)}">
      <span class="check-icon">${icon}</span>
      <span class="check-name">${escapeHtml(c.name)}</span>
      ${detail}
      ${fix}
    </li>`;
}

async function onPingClick() {
  const out = $("ping-result");
  out.textContent = "pinging…";
  const sentAt = Date.now();
  try {
    const r = await invoke<PingResult>("ping_agent");
    const rttMs = Date.now() - sentAt;
    out.textContent = `OK — agent_time=${r.agent_time} (round-trip ${rttMs} ms)`;
  } catch (err) {
    out.textContent = `error: ${String(err)}`;
  }
}

// Defensive escape for any string we splice into `innerHTML`.
// Covers the OWASP XSS prevention basics: `&` (encoding entity),
// `<` / `>` (tag delimiters), `"` and `'` (attribute delimiters),
// and `/` (closing-tag boundary). Overly broad for our own
// controlled inputs but the cost is rounding-error.
function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;")
    .replace(/\//g, "&#x2F;");
}

window.addEventListener("DOMContentLoaded", () => {
  $("ping-btn").addEventListener("click", onPingClick);

  // Delegated click handling: both the Health list (fix buttons) and
  // the runs list (kill buttons) are re-rendered via innerHTML, so a
  // single document-level listener survives the churn that per-element
  // listeners wouldn't.
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    const fixBtn = t.closest<HTMLButtonElement>(".fix-btn");
    if (fixBtn?.dataset.jobId) {
      // Disable on click so a rapid double-click can't spawn the job
      // twice before the first invoke resolves. The next Health
      // re-render (poll) restores a fresh, enabled button.
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
    // Notification ack — from the panel row or the emergency modal.
    // Both ack the notification for this OS user; disable on click so a
    // double-tap can't fire two acks before the first resolves.
    const ackBtn = t.closest<HTMLButtonElement>(
      ".notif-ack-btn, .emergency-ack-btn",
    );
    if (ackBtn?.dataset.notifId) {
      ackBtn.disabled = true;
      // Re-enable on failure so a transient error doesn't wedge the
      // button. On success the row/modal re-renders (button gone).
      ackNotification(ackBtn.dataset.notifId).catch(() => {
        ackBtn.disabled = false;
      });
      return;
    }
    // Emergency with no ack required: 閉じる just dismisses locally.
    const closeBtn = t.closest<HTMLElement>(".emergency-close-btn");
    if (closeBtn?.dataset.notifId) {
      dismissEmergencyModal(closeBtn.dataset.notifId);
      return;
    }
    // Job catalog: a tab switches the visible category…
    const tab = t.closest<HTMLElement>(".jobs-tab");
    if (tab?.dataset.category) {
      activeJobsTab = tab.dataset.category as JobCategory;
      renderJobsTabs();
      renderJobsList();
      return;
    }
    // …and a run button executes the job. Unlike the catalog (loaded
    // once, not polled), nothing re-renders this button, so disable it
    // only for the in-flight invoke and re-enable after, allowing reruns.
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

  // Agent→client pushes the backend forwards as `klp-notification`
  // events (#467). `jobs.progress` drives the remediation run rows
  // live; other methods (e.g. `state.changed`) can hook in here later.
  void listen<RpcNotification>("klp-notification", (event) => {
    // Guard the payload before touching `method`: a malformed / null
    // frame (shouldn't happen on typed IPC, but a single bad frame must
    // not break the listener for every future push) is dropped rather
    // than throwing on the dereference.
    const payload = event.payload as Partial<RpcNotification> | null;
    if (typeof payload?.method !== "string") return;
    // Each branch then guards its own cast.
    if (payload.method === "jobs.progress") {
      const p = payload.params as Partial<JobProgress> | null;
      if (!p?.run_id) return;
      handleProgress(p as JobProgress);
      return;
    }
    // `notifications.new` carries the full Notification body inline
    // (flattened on the wire), so no second round-trip is needed.
    if (payload.method === "notifications.new") {
      const n = payload.params as Partial<AppNotification> | null;
      if (!n?.id) return;
      handleNewNotification(n as AppNotification);
      return;
    }
  });

  // Reconnect lifecycle (#468). The Tauri supervisor reconnects when
  // the agent's pipe drops (e.g. the agent self-updated); these events
  // let the UI react instead of looking frozen.
  void listen("klp-connected", () => {
    // Fresh connection (initial or reconnect) — re-pull everything.
    // `jobsLoaded` / `notifSubscribed` are reset so the catalog reloads
    // and the notification subscription is re-established against the
    // new connection (the agent's subscription is per-connection).
    jobsLoaded = false;
    notifSubscribed = false;
    void renderStatus();
  });
  void listen("klp-disconnected", () => {
    // Connection dropped; the supervisor is reconnecting. Stop the
    // health poll and show a banner instead of silently-failing calls.
    if (healthTimer !== undefined) {
      window.clearInterval(healthTimer);
      healthTimer = undefined;
    }
    $("status").innerHTML =
      `<p class="error">エージェントとの接続が切れました。再接続しています…</p>`;
  });

  // Stuck-run watchdog tick (see checkStuckRuns). Once a minute is
  // plenty for a 15-minute deadline.
  window.setInterval(checkStuckRuns, 60_000);

  // Expire notifications off the panel / modal as their `expires_at`
  // passes, even while the app sits idle (see sweepExpired).
  window.setInterval(sweepExpired, 60_000);

  renderStatus();
});
