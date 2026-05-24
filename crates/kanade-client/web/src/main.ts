// Kanade Client App WebView entry point.
//
// Sprint 8 skeleton: on load, ask the Tauri backend for the
// cached handshake result, render "Connected to agent vX.Y.Z as
// DOMAIN\\user (pc_id)". Wire up a "Ping" button that
// round-trips system.ping through the backend's invoke handler
// and shows the agent's wall-clock.
//
// Future PRs add Health / Notifications / Jobs / Support tabs;
// for now the page is intentionally one-screen and dependency-
// light (no framework) so the next PR's UI redesign isn't
// fighting any priors.

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
  } catch (err) {
    status.innerHTML = `<p class="error">Agent unavailable: ${escapeHtml(String(err))}</p>
      <p class="muted">Retrying in 5 s…</p>`;
    // Crude retry loop; a proper PR adds a tauri event the backend
    // emits once the pipe lands.
    setTimeout(renderStatus, 5000);
  }
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

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

window.addEventListener("DOMContentLoaded", () => {
  $("ping-btn").addEventListener("click", onPingClick);
  renderStatus();
});
