// Kanade Client App WebView entry point.
//
// Sprint 8 skeleton: on load, ask the Tauri backend for the
// cached handshake result, render "Connected to agent vX.Y.Z as
// DOMAIN\\user (pc_id)". Wire up a "Ping" button that
// round-trips system.ping through the backend's invoke handler
// and shows the agent's wall-clock.
//
// The Health tab (#290) renders the agent's state.snapshot below;
// Notifications / Jobs / Support tabs land in future PRs. The page
// is intentionally one-screen and dependency-light (no framework)
// so a later UI redesign isn't fighting any priors.

import { invoke } from "@tauri-apps/api/core";

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
  // The remediation action needs `jobs.execute`, which isn't wired on
  // the agent yet (#291) — show the button so the intent is visible,
  // disabled until that lands.
  const fix = c.troubleshoot
    ? `<button class="fix-btn" disabled title="修復ジョブの実行は jobs.execute 実装後（#291）">修復する</button>`
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
  renderStatus();
});
