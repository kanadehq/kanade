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
    // The agent went away (service restart / crash) — a live request
    // is the only thing that detects it (`get_handshake` reads the
    // cached result, so it can't). Stop the poll so we don't hammer a
    // dead pipe, and correct the top status header, which would
    // otherwise still misleadingly read "Connected".
    //
    // We deliberately do NOT re-enter `renderStatus` here: its
    // `get_handshake` would succeed from cache and loop straight back
    // into a failing `renderHealth`. Auto-reconnect (re-establish the
    // pipe + a `klp-ready` event) lands with the push/reader-task
    // follow-up; until then the user relaunches the app.
    el.innerHTML = `<p class="error">ヘルス情報を取得できません: ${escapeHtml(String(err))}</p>`;
    $("status").innerHTML = `<p class="error">Agent connection lost: ${escapeHtml(String(err))}</p>`;
    if (healthTimer !== undefined) {
      window.clearInterval(healthTimer);
      healthTimer = undefined;
    }
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
    if (event.payload.method !== "jobs.progress") return;
    // Guard the cast: a malformed / null payload (shouldn't happen on
    // typed IPC, but a single bad frame must not break the listener
    // for every future push) is dropped rather than throwing.
    const p = event.payload.params as Partial<JobProgress> | null;
    if (!p?.run_id) return;
    handleProgress(p as JobProgress);
  });

  // Stuck-run watchdog tick (see checkStuckRuns). Once a minute is
  // plenty for a 15-minute deadline.
  window.setInterval(checkStuckRuns, 60_000);

  renderStatus();
});
