// Kanade Client App WebView entry point.
//
// Dashboard redesign: a slim header (brand + a single connection
// status dot), a left sidebar nav (ホーム / 通知 / 端末ヘルス + one
// entry per user-invokable job *category*), and a content area that
// switches views client-side. No framework — the existing IPC + render
// logic is reused as-is; only the layout / navigation is new.
//
// - `client:` jobs (jobs.list, #291) become the category nav items
//   (アップデート / 困ったとき / カタログ), grouped by `JobCategory`.
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
};

type JobsListResult = { items: UserInvokableJob[] };

// The raw `RpcNotification` the backend re-emits as a `klp-notification`
// Tauri event; we switch on `method`.
type RpcNotification = { jsonrpc: string; method: string; params: unknown };

// Per-category display metadata: sidebar label + default lucide icon
// (used both for the nav entry and as the per-job icon fallback). Order
// is the sidebar display order. `category` matches
// `kanade_shared::ipc::jobs::JobCategory`.
const CATEGORIES: { category: JobCategory; label: string; icon: string }[] = [
  { category: "software_update", label: "アップデート", icon: "download" },
  { category: "troubleshoot", label: "困ったとき", icon: "wrench" },
  { category: "catalog", label: "カタログ", icon: "package" },
];

function categoryMeta(c: JobCategory): { label: string; icon: string } {
  return CATEGORIES.find((m) => m.category === c) ?? { label: c, icon: "box" };
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

function isTerminal(status: RunStatus): boolean {
  return (
    status === "completed" || status === "failed" || status === "killed"
  );
}

function evictOldRuns(): void {
  if (runs.size <= MAX_RUNS) return;
  for (const [id, r] of runs) {
    if (runs.size <= MAX_RUNS) break;
    if (isTerminal(r.status)) runs.delete(id);
  }
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
  if (p.stdout_chunk) run.output += p.stdout_chunk;
  if (p.stderr_chunk) run.output += p.stderr_chunk;
  runs.set(p.run_id, run);
  renderRuns();
}

function activeRunCount(): number {
  let n = 0;
  for (const r of runs.values()) if (!isTerminal(r.status)) n++;
  return n;
}

function renderRuns(): void {
  evictOldRuns();
  const section = $("runs-section");
  section.hidden = runs.size === 0;
  if (runs.size > 0) {
    const list = [...runs.values()].reverse(); // newest first
    const container = $("runs");
    // Full render on any structural change; otherwise update each row in
    // place so a status/output tick doesn't blow away scroll / text
    // selection. A same-length set can still differ by key (an evict +
    // add in one pass), so verify every list row has a matching DOM node
    // before taking the in-place path — otherwise the new run would be
    // skipped and the evicted one would linger (Gemini #636).
    const hasAllRows = list.every((r) =>
      document.getElementById(`run-${r.runId}`),
    );
    if (container.children.length !== list.length || !hasAllRows) {
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
  updateRunsCard();
}

function renderRun(r: Run): string {
  const icon = RUN_STATUS_ICON[r.status] ?? "⏳";
  const label = RUN_STATUS_LABEL[r.status] ?? r.status;
  const kill =
    r.status === "running" || r.status === "queued"
      ? `<button class="kill-btn" data-run-id="${escapeHtml(r.runId)}">中止</button>`
      : "";
  const output = r.output.trim()
    ? `<pre class="run-output">${escapeHtml(r.output.slice(-4000))}</pre>`
    : "";
  return `
    <div class="run-row status-${escapeHtml(r.status)}" id="run-${escapeHtml(r.runId)}">
      <span class="run-icon">${icon}</span>
      <span class="run-name">${escapeHtml(r.label)}</span>
      <span class="run-status muted">${escapeHtml(label)}</span>
      ${kill}
      ${output}
    </div>`;
}

// ---- Job catalog (#291): category nav + per-category job list ----

const jobsByCategory = new Map<JobCategory, UserInvokableJob[]>();
let activeJobsTab: JobCategory = CATEGORIES[0].category;
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

// Inject one sidebar entry per category that actually has jobs (an empty
// category gets no menu item — that's the "category でメニュー化" ask).
function renderCategoryNav(): void {
  const withJobs = CATEGORIES.filter(
    (m) => (jobsByCategory.get(m.category)?.length ?? 0) > 0,
  );
  $("nav-jobs-sep").hidden = withJobs.length === 0;
  $("nav-categories").innerHTML = withJobs
    .map((m) => {
      const count = jobsByCategory.get(m.category)?.length ?? 0;
      return `
        <button class="nav-item" data-view="jobs" data-category="${m.category}">
          ${iconHtml(m.icon, m.icon, "nav-icon")}
          <span class="nav-label">${escapeHtml(m.label)}</span>
          <span class="nav-count muted">${count}</span>
        </button>`;
    })
    .join("");
  hydrateIcons();
}

function renderJobsList(): void {
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
  issued_at: string;
  issued_by?: string | null;
  expires_at?: string | null;
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

const notifications = new Map<string, AppNotification>();
// Ids whose body is expanded in the panel. A notification renders collapsed
// (title only) until the user clicks it open — opening an unread, ack-optional
// notification is what marks it read (the user can't have "seen" a body that
// was never expanded). Survives re-renders.
const expandedIds = new Set<string>();
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
    const target = pendingEmergencyId
      ? notifications.get(pendingEmergencyId)
      : null;
    if (pendingEmergencyId && target) {
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
    } else if (pendingEmergencyId) {
      // Launched for an id we don't have in history — reveal the window.
      void invoke("show_main_window").catch(() => {});
    } else {
      // Normal / reconnect launch (SPEC §2.12.8 recovery): an unread,
      // non-expired emergency whose live push arrived while the pipe was
      // down (so we never toasted it) is surfaced now from history. The
      // `toastedIds` guard keeps a reconnect from re-toasting one already
      // shown.
      for (const n of res.items) {
        if (
          n.priority === "emergency" &&
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

// ---- Emergency launch (#102): agent → `--show-notification <id>` ----

let pendingEmergencyId: string | null = null;
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
    pendingEmergencyId =
      (await invoke<string | null>("get_launch_notification")) ?? null;
  } catch (err) {
    console.error("get_launch_notification failed", err);
    pendingEmergencyId = null;
  }
}

// Surface a notification as a native OS toast — the whole point of OS
// toasts (vs an in-app toast or a modal) is that they show in Windows'
// notification area regardless of whether our window is visible/focused,
// so they never burst over whatever the user is doing (a meeting). ALL
// priorities go through here. Clicking the toast reveals the window
// focused on the notification in the panel (where its 確認 button lives).
// Falls back to revealing the window if toast permission is denied or the
// OS-toast call fails — so a notification is never silently lost.
async function surfaceOsToast(n: AppNotification): Promise<void> {
  const icon = PRIORITY_ICON[n.priority] ?? PRIORITY_ICON.unknown;
  // Mark as toasted up front so the reconnect-recovery loop won't
  // re-toast it (and a concurrent live push for the same id is a no-op).
  toastedIds.add(n.id);

  // Emergency → native WinRT toast (show_emergency_toast): it persists on
  // screen until dismissed (scenario=Reminder) and stays in the Action
  // Center, unlike the plugin's sendNotification which auto-dismisses in
  // ~7s. Clicking it (body or 確認 button) opens the client focused on this
  // notification via the kanade-client:// protocol (#647) — hence the id.
  // Fall through to the plugin path only if the native command fails.
  if (n.priority === "emergency") {
    try {
      await invoke("show_emergency_toast", {
        title: `${icon} ${n.title}`,
        body: n.body,
        id: n.id,
      });
      return;
    } catch (err) {
      console.error(
        "native emergency toast failed; falling back to plugin toast",
        err,
      );
    }
  }

  try {
    let granted = await isPermissionGranted();
    if (!granted) granted = (await requestPermission()) === "granted";
    if (!granted) {
      revealForEmergency(n);
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
      // the catch (revealForEmergency) and NO toast was ever sent — the
      // emergency silently fell back to a window on every desktop launch.
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
    revealForEmergency(n);
  }
}

// Fallback when the OS toast can't be shown (permission denied / failure):
// reveal the window ONLY for an emergency — stealing focus for a routine
// info/warn would defeat the non-intrusive goal (it's still in the panel).
function revealForEmergency(n: AppNotification): void {
  if (n.priority !== "emergency") return;
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
  // lingers while the user is looking straight at the body).
  expandedIds.add(id);
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

// Apply one `notifications.new` push: store, re-render, surface as a
// non-intrusive OS toast (no screen-grabbing modal, no in-app toast that
// only shows when the window is up). All priorities go through the same
// path; emergency only differs in the fallback (revealForEmergency).
function handleNewNotification(n: AppNotification): void {
  notifications.set(n.id, n);
  renderNotifications();
  if (isExpired(n)) return;
  void surfaceOsToast(n);
}

// Single-instance forward (#624): a SECOND `kanade-client` launched with
// `--show-notification <id>` (the agent's per-emergency fallback) was
// collapsed into this already-running instance, which forwarded the id
// here. Toast that emergency from here so a new hidden process never piles
// up. If we already toasted it (its live push beat the forward), do
// nothing — `surfaceOsToast` only *records* into `toastedIds`, it doesn't
// guard on it, so the caller must (else we'd double-toast). If we don't
// have it yet (forwarded before its push), re-pull history once and
// retry; failing that, reveal the window so the forward isn't a silent
// no-op.
async function surfaceForwardedEmergency(id: string): Promise<void> {
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
    console.error("forwarded emergency: list re-pull failed", err);
  }
  if (!tryToast()) {
    void invoke("show_main_window").catch(() => {});
  }
}

// Presence-driven re-surface (#647): the agent forwarded `--resurface` because
// the user just became present (logon / unlock) after emergencies they
// couldn't see — sent while signed out, or delivered to the Action Center
// while the screen was locked. Re-pull the freshest unread set, then re-toast
// every unread, unexpired emergency, DELIBERATELY bypassing the toastedIds
// dedup (the whole point is to re-pop ones already silently delivered).
// info/warn stay passive.
async function resurfaceAllEmergencies(): Promise<void> {
  try {
    const res = await invoke<NotificationsListResult>("notifications_list", {
      filter: "all",
      cursor: null,
    });
    for (const n of res.items) notifications.set(n.id, n);
    renderNotifications();
  } catch (err) {
    console.error("resurface: list re-pull failed", err);
  }
  for (const n of notifications.values()) {
    if (n.priority === "emergency" && !n.acked_at && !isExpired(n)) {
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
    }
  }
  if (notifications.size <= MAX_NOTIFICATIONS) return;
  for (const [id, n] of notifications) {
    if (notifications.size <= MAX_NOTIFICATIONS) break;
    if (n.acked_at) {
      notifications.delete(id);
      expandedIds.delete(id);
    }
  }
}

function renderNotification(n: AppNotification): string {
  const icon = PRIORITY_ICON[n.priority] ?? PRIORITY_ICON.unknown;
  const acked = !!n.acked_at;
  const expanded = expandedIds.has(n.id);
  const meta = [
    n.issued_by ? `送信元: ${escapeHtml(n.issued_by)}` : "",
    fmtTime(n.issued_at),
  ]
    .filter(Boolean)
    .join(" · ");
  // Ack-required notifications clear their unread state via the explicit
  // 確認 button; ack-optional ones clear it by being opened (read). So only
  // require_ack carries an action control here.
  const ackCtl =
    n.require_ack && !acked
      ? `<button class="notif-ack-btn" data-notif-id="${escapeHtml(n.id)}">確認</button>`
      : n.require_ack && acked
        ? `<span class="notif-acked muted">✓ 確認済み</span>`
        : "";
  // An unread dot makes the badge count legible per-row; it clears the
  // moment the notification is read/acked.
  const unreadDot = acked ? "" : `<span class="notif-dot" aria-hidden="true"></span>`;
  const classes = [
    "notif-row",
    `priority-${escapeHtml(n.priority)}`,
    acked ? "acked" : "unread",
    expanded ? "expanded" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return `
    <div id="cnotif-${escapeHtml(n.id)}" class="${classes}">
      <span class="notif-icon">${icon}</span>
      <div class="notif-main">
        <div class="notif-head notif-toggle" data-notif-toggle="${escapeHtml(n.id)}" role="button" tabindex="0" aria-expanded="${expanded}">
          ${unreadDot}
          <span class="notif-title">${escapeHtml(n.title)}</span>
          <span class="notif-prio muted">${escapeHtml(PRIORITY_LABEL[n.priority] ?? PRIORITY_LABEL.unknown)}</span>
          <span class="notif-chevron" aria-hidden="true">▸</span>
        </div>
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
    } else {
      text = "✅ 全て正常";
      level = "ok";
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
  const fix = c.troubleshoot
    ? `<button class="fix-btn" data-job-id="${escapeHtml(c.troubleshoot)}" data-check="${escapeHtml(c.name)}">修復する</button>`
    : "";
  // Show the operator's human title when set; fall back to the slug.
  const title = c.label && c.label.trim() ? c.label : c.name;
  return `
    <li class="check-row status-${escapeHtml(c.status)}">
      <span class="check-icon">${icon}</span>
      <span class="check-name">${escapeHtml(title)}</span>
      ${detail}
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
      e.preventDefault();
      toggleNotification(toggle.dataset.notifToggle);
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
    if (id) void surfaceForwardedEmergency(id);
  });

  // Presence-driven re-surface (#647): a second `--resurface` launch was
  // collapsed into this instance; re-pop every unread emergency.
  void listen("klp-resurface", () => {
    void resurfaceAllEmergencies();
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
