/**
 * Mock backend for the demo SPA (`cargo make demo`).
 *
 * Serves the `/api/*` surface the SPA reads, backed by the invented
 * fleet in `fleet.ts`. Nothing here talks to NATS, SQLite or a real
 * agent — the point is to bring the UI up on a laptop with no
 * infrastructure at all, for screenshots, demos and design work.
 *
 * Wired in via Vite's dev proxy: `cargo make demo` starts this on
 * :8082 and points `BACKEND_PROXY` at it, so the SPA itself is
 * untouched and unaware. No demo-mode branch ships in the product.
 *
 * Auth is deliberately a no-op: any username/password is accepted and
 * every request is treated as an admin. A demo that makes you find a
 * password is a demo nobody runs.
 *
 * Unimplemented routes answer 200 with `[]` and log a line, so a page
 * that needs more than the demo covers says so in the terminal instead
 * of erroring in the browser. Detail routes 404 on an unknown id
 * instead — see the note next to the catch-all.
 */

import {
  CHECKS,
  CURRENT_VERSION,
  diskBucket,
  FLEET,
  JOBS,
  OFFLINE,
  ONLINE,
  osEolBucket,
  OS_EOL_DATE,
  rng,
  BACKEND_SIGNING_KEY,
  SITES,
  DEPARTMENTS,
} from './fleet';

const PORT = Number(process.env.DEMO_API_PORT ?? 8082);

// ---------------------------------------------------------------- utils

const iso = (msAgo: number) => new Date(Date.now() - msAgo).toISOString();
const isoIn = (msAhead: number) => new Date(Date.now() + msAhead).toISOString();

function json(body: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(body), {
    ...init,
    headers: { 'Content-Type': 'application/json', ...(init.headers ?? {}) },
  });
}

/** Evenly spaced series ending at `now`, for the sparkline cards. */
function series(
  hours: number,
  stepMinutes: number,
  fn: (i: number, r: () => number) => number | null,
): Array<{ at: string; value: number | null }> {
  const r = rng(0x51de);
  const n = Math.floor((hours * 60) / stepMinutes);
  const out: Array<{ at: string; value: number | null }> = [];
  for (let i = 0; i < n; i++) {
    out.push({
      at: iso((n - 1 - i) * stepMinutes * 60 * 1000),
      value: fn(i, r),
    });
  }
  return out;
}

// ------------------------------------------------------------ endpoints

type Handler = (req: Request, url: URL, m: RegExpMatchArray) => Response | Promise<Response>;

const ROUTES: Array<[string, RegExp, Handler]> = [];
const get = (p: RegExp, h: Handler) => ROUTES.push(['GET', p, h]);
const post = (p: RegExp, h: Handler) => ROUTES.push(['POST', p, h]);

// ---- auth ----

post(/^\/api\/auth\/login$/, () =>
  json({
    token: 'demo-token',
    role: 'admin',
    must_change_pw: false,
    exp: Math.floor((Date.now() + 12 * 3600 * 1000) / 1000),
  }),
);

// TOTP enrolment (#1192) is stateful here so the whole flow is walkable in
// the demo: `init` hands out a fixed candidate secret, `verify` flips what
// `me` reports, `disable` clears it. Nothing is validated — any code is
// accepted — because the point is to exercise the screens, not to be an
// authenticator. The secret is the RFC 4226 test vector, not a live one.
let mfaEnabled = false;
const DEMO_MFA_SECRET = 'JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP';

get(/^\/api\/auth\/me$/, () =>
  json({
    username: 'demo',
    role: 'admin',
    must_change_pw: false,
    mfa_enabled: mfaEnabled,
    allowed_features: null,
  }),
);

post(/^\/api\/auth\/mfa\/init$/, () =>
  json({
    secret: DEMO_MFA_SECRET,
    otpauth_url:
      `otpauth://totp/kanade:demo?secret=${DEMO_MFA_SECRET}` +
      '&issuer=kanade&algorithm=SHA1&digits=6&period=30',
  }),
);

post(/^\/api\/auth\/mfa\/verify$/, () => {
  mfaEnabled = true;
  return new Response(null, { status: 204 });
});

post(/^\/api\/auth\/mfa\/disable$/, () => {
  mfaEnabled = false;
  return new Response(null, { status: 204 });
});

get(/^\/api\/version$/, () => json({ version: CURRENT_VERSION }));

// ---- fleet roster ----

/** AgentRow, as `GET /api/agents` hands it to the Agents page. */
function agentRow(p: (typeof FLEET)[number]) {
  // Command-signing state (#1165 / #1250 / #1253 / #1260). Omitting these
  // is not neutral: `signingState()` reads a missing `command_keys` as
  // `unknown` — "this agent predates the field, upgrade it first" — so an
  // incomplete fixture does not leave the column blank, it fills the page
  // with that one verdict. Which is exactly how it looked.
  return {
    pc_id: p.pc_id,
    hostname: p.hostname,
    os_family: p.os_family,
    agent_version: p.agent_version,
    last_heartbeat: iso(p.heartbeat_ms_ago),
    updated_at: iso(p.heartbeat_ms_ago),
    agent_cpu_pct: p.agent_cpu_pct,
    agent_rss_bytes: p.agent_rss_bytes,
    agent_disk_read_bytes: p.agent_rss_bytes * 12,
    agent_disk_written_bytes: p.agent_rss_bytes * 4,
    quarantined_versions: [],
    last_logon_user: p.last_logon_user,
    last_logon_display_name: p.last_logon_display_name,
    // `agent_meta` is for what the MACHINE CANNOT KNOW about itself —
    // whose desk it is on, which cost centre pays for it, why it is an
    // exception. `model` and `serial` used to be in here, and they are
    // exactly the wrong example: the inventory probe already reports both,
    // and the PC detail page shows them as 機種 / シリアル. Duplicating
    // them here teaches that this card is a second place to keep facts the
    // fleet already collects, which is the opposite of the point.
    meta: [
      { key: 'display_name', value: p.last_logon_display_name },
      { key: 'email', value: p.email },
      { key: 'department', value: p.dept },
      { key: 'site', value: p.site },
      { key: 'asset_tag', value: `A-${p.serial.slice(-6)}` },
      // Lease end is the clearest example of the category: it lives in a
      // contract, so no probe can ever discover it, and it is always set
      // rather than blank — an empty row would read as an unfinished card.
      { key: 'lease_end', value: `20${27 + (p.serial.charCodeAt(p.serial.length - 1) % 3)}-03-31` },
    ],
    // Both are optional on the wire and their ABSENCE is meaningful, so
    // they are spread in rather than set to null: `command_keys: undefined`
    // and a missing key are the same thing to the page, but `null` is not.
    command_keys: p.command_keys,
    enforcing: p.enforcing,
  };
}

get(/^\/api\/agents$/, (_req, url) => {
  const q = (url.searchParams.get('q') ?? '').toLowerCase();
  const status = url.searchParams.get('status');
  const limit = Number(url.searchParams.get('limit') ?? '50');
  const offset = Number(url.searchParams.get('offset') ?? '0');

  let rows = FLEET;
  if (status === 'online') rows = rows.filter((p) => p.online);
  if (status === 'offline') rows = rows.filter((p) => !p.online);
  if (q) {
    rows = rows.filter((p) =>
      [p.pc_id, p.last_logon_display_name, p.dept, p.site, p.model].some((v) =>
        v.toLowerCase().includes(q),
      ),
    );
  }

  const page = rows.slice(offset, offset + limit).map(agentRow);
  return json(page, {
    headers: {
      'X-Total-Count': String(rows.length),
      'X-Online-Count': String(ONLINE.length),
      'X-Offline-Count': String(OFFLINE.length),
    },
  });
});

get(/^\/api\/agents\/versions$/, () => {
  const byVersion = new Map<string, { total: number; active: number }>();
  for (const p of FLEET) {
    const e = byVersion.get(p.agent_version) ?? { total: 0, active: 0 };
    e.total++;
    if (p.online) e.active++;
    byVersion.set(p.agent_version, e);
  }
  return json(
    [...byVersion.entries()]
      .map(([version, c]) => ({ version, ...c }))
      .sort((a, b) => b.total - a.total),
  );
});

get(/^\/api\/agents\/meta-keys$/, () =>
  json(['display_name', 'email', 'department', 'site', 'model', 'serial']),
);

// Per-PC routes. A parameterised pattern like `([^/]+)` would swallow
// the literal siblings above (`versions`, `meta-keys`) and below
// (`releases`) — so the dispatcher matches every literal route before
// any parameterised one, whatever order they were registered in. See
// `byLiteralFirst` at the bottom of this file.

const findPc = (id: string) => FLEET.find((p) => p.pc_id.toLowerCase() === id.toLowerCase());

get(/^\/api\/agents\/([^/]+)$/, (_req, _url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  return json(agentRow(p));
});

get(/^\/api\/agents\/([^/]+)\/meta$/, (_req, _url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  return json({ entries: agentRow(p).meta });
});

get(/^\/api\/agents\/([^/]+)\/perf$/, (_req, _url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  const hours = 24;
  const stepMinutes = 5;
  const r = rng(p.mem_total_bytes % 100_000);
  const n = Math.floor((hours * 60) / stepMinutes);
  const points = [];
  for (let i = 0; i < n; i++) {
    // Office-hours shaped, so the per-PC chart lines up with the
    // swimlane's active spans rather than being uniform noise.
    const hour = new Date(Date.now() - (n - 1 - i) * stepMinutes * 60_000).getHours();
    const busy = hour >= 9 && hour <= 18;
    points.push({
      at: iso((n - 1 - i) * stepMinutes * 60_000),
      cpu_pct: Math.round((busy ? 14 + r() * 38 : 2 + r() * 6) * 10) / 10,
      mem_used_bytes: Math.round(p.mem_used_bytes * (0.9 + r() * 0.2)),
      mem_total_bytes: p.mem_total_bytes,
      swap_used_bytes: Math.round(p.mem_total_bytes * 0.04 * r()),
      swap_total_bytes: Math.round(p.mem_total_bytes * 0.25),
      disk_read_bytes_per_sec: Math.round((busy ? 4_000_000 : 200_000) * r()),
      disk_written_bytes_per_sec: Math.round((busy ? 2_500_000 : 120_000) * r()),
      net_rx_bytes_per_sec: Math.round((busy ? 3_000_000 : 90_000) * r()),
      net_tx_bytes_per_sec: Math.round((busy ? 900_000 : 40_000) * r()),
    });
  }
  return json({
    pc_id: p.pc_id,
    from: iso(hours * 3600 * 1000),
    to: iso(0),
    step_seconds: stepMinutes * 60,
    points,
  });
});

// `[name, RSS MiB, CPU % ceiling]`. The CPU ceiling is per process rather
// than one number for all of them: a flat ceiling had `kanade-agent.exe`
// drawing up to 60 % CPU, which both contradicts the 0.9 % the Agents page
// reports for the agent and is a poor advertisement for a management agent.
// Ceilings are ordered the way a real desktop looks — a browser can exceed
// 100 % on a multi-core box, an idle helper cannot.
const TOP_PROCESSES = [
  ['chrome.exe', 5_400, 180],
  ['Teams.exe', 3_100, 45],
  ['EXCEL.EXE', 1_900, 35],
  ['OUTLOOK.EXE', 1_400, 20],
  ['Code.exe', 1_250, 40],
  ['MsMpEng.exe', 620, 25],
  ['explorer.exe', 320, 8],
  ['kanade-agent.exe', 42, 2],
] as const;

get(/^\/api\/agents\/([^/]+)\/processes$/, (_req, _url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  const r = rng(0x9a0c);
  return json({
    pc_id: p.pc_id,
    latest_at: iso(90 * 1000),
    processes: TOP_PROCESSES.map(([name, mb, cpuMax], i) => ({
      pid: 1000 + i * 137,
      name,
      // Never below a tenth of the ceiling, so a process does not flicker
      // to 0 % between reloads and read as "not running".
      cpu_pct: Math.round(cpuMax * (0.1 + r() * 0.9) * 10) / 10,
      rss_bytes: mb * 1024 * 1024,
      disk_read_bytes_per_sec: Math.round(r() * 1_200_000),
      disk_written_bytes_per_sec: Math.round(r() * 600_000),
    })),
  });
});

get(/^\/api\/agents\/([^/]+)\/processes\/timeline$/, (_req, url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  const metric = url.searchParams.get('metric') ?? 'rss_bytes';
  const names = TOP_PROCESSES.map(([n]) => n);
  const r = rng(0x77e1);
  const n = 96;
  return json({
    pc_id: p.pc_id,
    metric,
    from: iso(8 * 3600 * 1000),
    to: iso(0),
    step_seconds: 300,
    names,
    points: Array.from({ length: n }, (_, i) => ({
      at: iso((n - 1 - i) * 300_000),
      values: Object.fromEntries(
        TOP_PROCESSES.map(([name, mb, cpuMax]) => [
          name,
          // The SPA's metric values are `cpu | rss | disk_read | disk_written`
          // (`ChartMetric` in AgentProcessSection.tsx). Comparing against
          // `cpu_pct` never matched, so the CPU chart was fed RSS *bytes*
          // and plotted them on an axis labelled `%` — billions of percent.
          metric === 'cpu'
            ? Math.round(cpuMax * (0.1 + r() * 0.9) * 10) / 10
            : metric === 'disk_read' || metric === 'disk_written'
              ? Math.round(r() * (metric === 'disk_read' ? 1_200_000 : 600_000))
              : Math.round(mb * 1024 * 1024 * (0.8 + r() * 0.4)),
        ]),
      ),
    })),
  });
});

/**
 * The agent's own log tail — plain text, newest last, as the agent
 * writes it (`tracing` with a level and a target per line).
 *
 * Generated backwards from "now" so the tail always ends at the
 * present, and shaped like a quiet, healthy agent: a heartbeat every
 * minute, the scheduler firing the jobs this fleet actually runs, and
 * the occasional NATS reconnect. That last one matters — an operator
 * reading a log wants to know what NORMAL looks like, and a reconnect
 * on a laptop that slept is normal.
 */
function agentLog(p: (typeof FLEET)[number], tail: number): string {
  const lines: string[] = [];
  const at = (msAgo: number) =>
    new Date(Date.now() - msAgo).toISOString().replace('T', ' ').slice(0, 23);
  const cycle = [
    ['INFO', 'kanade_agent::heartbeat', `heartbeat sent pc_id=${p.pc_id} version=${p.agent_version}`],
    ['DEBUG', 'kanade_agent::scheduler', 'tick: 8 schedules registered, 0 due'],
    ['INFO', 'kanade_agent::commands', 'exec start job_id=defender-status request_id=6f1a2c3d'],
    ['INFO', 'kanade_agent::process', 'spawn powershell -NoProfile -File defender-status.ps1'],
    ['INFO', 'kanade_agent::commands', 'exec done job_id=defender-status exit_code=0 elapsed=1.42s'],
    ['DEBUG', 'kanade_agent::outbox', 'drained 1 result, 0 pending'],
    ['INFO', 'kanade_agent::idle_sampler', 'state=active last_input=3s'],
    ['INFO', 'kanade_agent::commands', 'exec start job_id=app-usage request_id=91be44f0'],
    ['INFO', 'kanade_agent::commands', 'exec done job_id=app-usage exit_code=0 elapsed=0.31s'],
    ['DEBUG', 'kanade_agent::obs_outbox', 'published 12 obs events subject=obs.' + p.pc_id],
    ['INFO', 'kanade_agent::winlog', 'collected 4 events source=winlog:System'],
    ['WARN', 'async_nats', 'connection lost, reconnecting in 1s'],
    ['INFO', 'async_nats', 'reconnected to nats://kanade.example.co.jp:4222'],
    ['INFO', 'kanade_agent::config', 'effective target_version=' + CURRENT_VERSION + ' (global)'],
  ] as const;

  for (let i = tail - 1; i >= 0; i--) {
    const [level, target, msg] = cycle[(tail - 1 - i) % cycle.length]!;
    lines.push(`${at(i * 37_000)}  ${level.padEnd(5)} ${target}: ${msg}`);
  }
  return lines.join('\n') + '\n';
}

get(/^\/api\/agents\/([^/]+)\/logs$/, (_req, url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  const tail = Math.min(Math.max(Number(url.searchParams.get('tail') ?? '200'), 1), 2000);
  return new Response(agentLog(p, tail), {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
});

get(/^\/api\/agents\/([^/]+)\/groups$/, (_req, _url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  return json({ groups: p.groups });
});

// ---- config (global defaults / per-PC scope / resolved) ----

/** What an agent actually ends up running with, after the global →
 *  group → per-PC layers are folded together. */
const EFFECTIVE_CONFIG = {
  target_version: CURRENT_VERSION,
  target_version_jitter: '30m',
  heartbeat_interval: '60s',
  host_perf_interval: '60s',
  process_perf_enabled: false,
  process_perf_expires_at: null,
  process_perf_top_n: 8,
  // null = nothing pinned, so the Client App uses its own built-in
  // product name. The Config page spells that fallback out in its help
  // text, so pinning a value here would contradict the copy next to it.
  client_display_name: null,
};

get(/^\/api\/config$/, () =>
  // The global scope pins only what the operator has actually set;
  // everything else inherits, which is the shape the Config page's
  // "inherit" placeholders are written against.
  json({ target_version: CURRENT_VERSION, target_version_jitter: '30m' }),
);

get(/^\/api\/config\/defaults$/, () => json(EFFECTIVE_CONFIG));

get(/^\/api\/agents\/([^/]+)\/effective_config$/, (_req, _url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return new Response('agent not found', { status: 404 });
  return json({
    pc_id: p.pc_id,
    effective: { ...EFFECTIVE_CONFIG, target_version: p.agent_version },
    warnings: [],
  });
});

get(/^\/api\/pcs\/([^/]+)\/config$/, () =>
  // No per-PC override in the demo fleet — an empty scope is the normal
  // case, and it's what makes the detail page show "inherited".
  json({}),
);

// ---- fleet health (Dashboard hero card) ----

get(/^\/api\/health\/fleet$/, () =>
  json({
    status: 'ok',
    agents: { known: FLEET.length, active: ONLINE.length, stale: OFFLINE.length },
    // Counted, never quoted. Both of these used to be literals and both
    // were wrong the moment the fixtures moved underneath them.
    jetstream: {
      all_ok: true,
      healthy: JETSTREAM_RESOURCE_COUNT,
      total: JETSTREAM_RESOURCE_COUNT,
      missing: [],
    },
    recent_results: {
      window_hours: 24,
      total: RESULTS.length,
      failed: RESULTS.filter((x) => x.exit_code !== 0).length,
    },
    observed_at: iso(0),
  }),
);

get(/^\/api\/health\/scan_durations$/, () => {
  const r = rng(0x5ca4);
  return json(
    JOBS.map((j) => {
      const p50 = Math.round(180 + r() * 2600);
      return {
        job_id: j.id,
        count: Math.round(200 + r() * 900),
        min_ms: Math.round(p50 * 0.4),
        p50_ms: p50,
        p95_ms: Math.round(p50 * 2.1),
        p99_ms: Math.round(p50 * 3.4),
        max_ms: Math.round(p50 * 4.8),
        mean_ms: Math.round(p50 * 1.2),
        max_result_id: `res-${j.id}-max`,
      };
    }).sort((a, b) => b.p95_ms - a.p95_ms),
  );
});

// ---- recent results / audit ----

/**
 * The run history, built ONCE and shared by the list and the detail
 * endpoint. Deriving the detail independently would let the two drift:
 * clicking "defender-status · KANADE-PC-0124" in the list would open a
 * detail page for some other job on some other host, because the list's
 * PRNG walk isn't reproducible from a result_id alone. The result_id
 * indexes straight into this array instead.
 */
/**
 * Sized to what 248 hosts actually produce in a day, not to what is
 * convenient to generate.
 *
 * It was 80, while the fleet-health card separately claimed 1,864 runs and
 * 7 failures in the last 24h — two hand-written numbers with nothing tying
 * them to the rows underneath. Clicking "failed: 7" opened a list of 4.
 * Both figures are now derived from this array, so they cannot disagree
 * again; see `/api/health/fleet`.
 */
const RESULT_COUNT = 1864;

type DemoResult = {
  result_id: string;
  request_id: string;
  job_id: string;
  pc_id: string;
  exit_code: number;
  finished_ms_ago: number;
  duration_ms: number;
  stdout: string;
  stderr: string;
};

/**
 * Plausible stdout per job. Inventory jobs print the JSON object the
 * projector upserts; check jobs print a `check:` payload.
 *
 * `app-usage` is the exception, and it is NOT an oversight: it carries
 * an `emit:` hint, and on a clean exit the agent parses its NDJSON into
 * obs_events and then **blanks stdout on the ExecResult**
 * (`kanade-agent/src/commands.rs:704-716`) — re-shipping ~50 event lines
 * per PC per day through `execution_results.stdout` would swamp a table
 * built for one row per run. A failed emit run keeps its stdout, so the
 * operator can see the partial output that broke it. Printing a tidy
 * summary object here would show operators a shape the product never
 * produces.
 */
const EMIT_JOBS = new Set(['app-usage', 'web-history']);

function resultStdout(jobId: string, pc: (typeof FLEET)[number], failed: boolean): string {
  if (EMIT_JOBS.has(jobId)) {
    if (!failed) return '';
    // Kept-on-failure: NDJSON, one ObsEvent per line, cut off mid-line
    // where the script died.
    const line = (payload: unknown, kind: string, source: string) =>
      JSON.stringify({ pc_id: pc.pc_id, at: iso(90 * 60 * 1000), kind, source, payload });
    const lines =
      jobId === 'web-history'
        ? [
            line({ domain: 'portal.example.co.jp', title: '社内ポータル', browser: 'edge' }, 'web_visit', 'agent:web_history'),
            line({ domain: 'outlook.office.com', title: 'Outlook', browser: 'edge' }, 'web_visit', 'agent:web_history'),
          ]
        : [
            line({ app: 'EXCEL.EXE', seconds: 300 }, 'app_sample', 'agent:app_usage'),
            line({ app: 'chrome.exe', seconds: 180 }, 'app_sample', 'agent:app_usage'),
          ];
    return `${lines.join('\n')}\n{"pc_id":"${pc.pc_id}","at":"`;
  }
  if (failed) return '';
  switch (jobId) {
    case 'inventory-basic':
      return JSON.stringify(
        {
          hostname: pc.hostname,
          os_caption: pc.os_caption,
          os_version: pc.os_version,
          os_build: pc.os_build,
          model: pc.model,
          serial: pc.serial,
          mem_total_bytes: pc.mem_total_bytes,
          disk_total_bytes: pc.disk_total_bytes,
          disk_free_bytes: pc.disk_free_bytes,
        },
        null,
        2,
      );
    case 'inventory-apps':
      return JSON.stringify({ apps: [{ name: 'Google Chrome', version: '138.0.7204.94' }, '…'] }, null, 2);
    case 'defender-status':
      return JSON.stringify(
        { check: { name: 'defender_realtime', status: 'ok', detail: null } },
        null,
        2,
      );
    case 'bitlocker-status':
      return JSON.stringify(
        { check: { name: 'bitlocker', status: 'ok', detail: 'C: 保護は有効 (XtsAes256)' } },
        null,
        2,
      );
    case 'windows-update':
      return JSON.stringify(
        { check: { name: 'windows_update', status: 'ok', detail: '未適用の重要な更新はありません' } },
        null,
        2,
      );
    case 'disk-space':
      return JSON.stringify(
        {
          check: {
            name: 'disk_free',
            status: 'ok',
            detail: `空き容量 ${Math.round((pc.disk_free_bytes / pc.disk_total_bytes) * 100)}%`,
          },
        },
        null,
        2,
      );
    default:
      return JSON.stringify({ check: { name: jobId.replace(/-/g, '_'), status: 'ok' } }, null, 2);
  }
}

function buildResults(): DemoResult[] {
  const r = rng(0x7e50);
  const out: DemoResult[] = [];
  for (let i = 0; i < RESULT_COUNT; i++) {
    const job = JOBS[Math.floor(r() * JOBS.length)]!;
    const pc = FLEET[Math.floor(r() * FLEET.length)]!;
    // A few red rows: the failure path is half of what the product is
    // for, so a demo that hides it undersells it. A fixed stride rather
    // than a PRNG draw, so the count is stable across restarts and the
    // health card's figure is reproducible.
    const failed = i % 266 === 3;
    out.push({
      result_id: `res-${String(i).padStart(6, '0')}`,
      request_id: `${Math.floor(r() * 0xffffffff).toString(16).padStart(8, '0')}-4f2a-demo`,
      job_id: job.id,
      pc_id: pc.pc_id,
      exit_code: failed ? 1 : 0,
      finished_ms_ago: (i + 1) * 97 * 1000,
      duration_ms: Math.round(400 + r() * 4200),
      stdout: resultStdout(job.id, pc, failed),
      stderr: failed
        ? `${job.id} : アクセスが拒否されました。\n` +
          '発生場所 C:\\ProgramData\\Kanade\\scripts\\' + job.id + '.ps1:24 文字:5\n' +
          '+     $out = Get-CimInstance -ClassName Win32_EncryptableVolume ...\n' +
          '+     ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\n' +
          '    + CategoryInfo          : PermissionDenied: (:) [Get-CimInstance], CimException\n' +
          '    + FullyQualifiedErrorId : HRESULT 0x80070005,Microsoft.Management.Infrastructure.CimCmdlets.GetCimInstanceCommand\n'
        : '',
    });
  }
  return out;
}

const RESULTS = buildResults();

/**
 * The list shape. Carries a CLIPPED stdout/stderr preview plus the
 * `*_truncated` flags, which is what the Activity table renders inline —
 * omitting them left every row's output column blank, so a page whose
 * whole job is "what did this run print?" looked like nothing ever
 * prints anything. The flags drive the "show more" affordance, which
 * then re-fetches the full body from the detail route.
 */
const PREVIEW_CHARS = 200;

function resultListRow(x: DemoResult) {
  return {
    result_id: x.result_id,
    request_id: x.request_id,
    exec_id: `exec-${x.result_id.slice(4)}`,
    job_id: x.job_id,
    pc_id: x.pc_id,
    exit_code: x.exit_code,
    stdout: x.stdout.slice(0, PREVIEW_CHARS),
    stderr: x.stderr.slice(0, PREVIEW_CHARS),
    stdout_truncated: x.stdout.length > PREVIEW_CHARS,
    stderr_truncated: x.stderr.length > PREVIEW_CHARS,
    started_at: iso(x.finished_ms_ago + x.duration_ms),
    finished_at: iso(x.finished_ms_ago),
  };
}

get(/^\/api\/results$/, (_req, url) => {
  const limit = Number(url.searchParams.get('limit') ?? '50');
  const offset = Number(url.searchParams.get('offset') ?? '0');
  const jobId = url.searchParams.get('job_id');
  const pcId = url.searchParams.get('pc_id');
  const status = url.searchParams.get('status');

  let rows = RESULTS;
  if (jobId) rows = rows.filter((x) => x.job_id === jobId);
  if (pcId) rows = rows.filter((x) => x.pc_id.toLowerCase().includes(pcId.toLowerCase()));
  if (status === 'failure') rows = rows.filter((x) => x.exit_code !== 0);
  if (status === 'success') rows = rows.filter((x) => x.exit_code === 0);

  return json(rows.slice(offset, offset + limit).map(resultListRow), {
    headers: { 'X-Total-Count': String(rows.length) },
  });
});

get(/^\/api\/results\/([^/]+)$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  const x = RESULTS.find((y) => y.result_id === id);
  // A detail route must 404 on an unknown id rather than fall through to
  // the `[]` catch-all: the page reads the body as an object, so `[]`
  // surfaces as "Cannot read properties of undefined" instead of the
  // SPA's own not-found handling.
  if (!x) return new Response('result not found', { status: 404 });
  return json({
    ...resultListRow(x),
    // The detail route ships the FULL body, not the preview.
    stdout: x.stdout,
    stderr: x.stderr,
    stdout_truncated: false,
    stderr_truncated: false,
    version: '1',
  });
});

get(/^\/api\/results\/([^/]+)\/tail$/, (_req, _url, m) => {
  const x = RESULTS.find((y) => y.result_id === decodeURIComponent(m[1]!));
  if (!x) return new Response('result not found', { status: 404 });
  return json({
    running: false,
    live: false,
    stdout: x.stdout,
    stderr: x.stderr,
    stdout_truncated: false,
    stderr_truncated: false,
    exit_code: x.exit_code,
  });
});

/**
 * The audit trail.
 *
 * Every row carries a `payload`, because that column is the page: actor +
 * action + target says something happened, and only the payload says WHAT.
 * An earlier fixture omitted it entirely, and the failure was silent in the
 * worst way — the cell renders `JSON.stringify(undefined)`, which is not a
 * string, so every row showed an expander that opened onto nothing. The page
 * looked implemented and answered no question.
 *
 * The shapes differ per action on purpose. A uniform `{ ok: true }` would
 * fill the column without earning it; what an operator actually wants from
 * this page is "which job, against how many hosts, on whose authority", and
 * that is a different set of keys for a rollout than for a notification.
 */
const AUDIT_EVENTS: Array<{
  actor: string;
  action: string;
  target: string;
  payload: Record<string, unknown>;
}> = [
  {
    actor: 'scheduler',
    action: 'exec',
    target: 'inventory-basic',
    payload: { schedule: 'inventory-basic', cron: '0 */6 * * *', targets: ONLINE.length, dispatched: ONLINE.length },
  },
  {
    actor: 'sato.kenji',
    action: 'job.publish',
    target: 'edge-extensions',
    payload: { job: 'edge-extensions', version: '1.3.0', groups: ['全社'], run_as: 'user', timeout_secs: 600 },
  },
  {
    actor: 'scheduler',
    action: 'exec',
    target: 'app-usage',
    payload: { schedule: 'app-usage', cron: '*/30 * * * *', targets: ONLINE.length, dispatched: ONLINE.length },
  },
  {
    actor: 'sato.kenji',
    action: 'notification.publish',
    target: '【重要】月次セキュリティ更新の適用について',
    payload: { id: 'ntf-0003', priority: 'warn', require_ack: true, toast: true, target: { all: true }, expires_in_days: 2 },
  },
  {
    actor: 'scheduler',
    action: 'exec',
    target: 'defender-status',
    payload: { schedule: 'defender-status', cron: '15 * * * *', targets: ONLINE.length, dispatched: ONLINE.length },
  },
  {
    actor: 'tanaka.misaki',
    action: 'agent.rollout',
    target: CURRENT_VERSION,
    payload: { version: CURRENT_VERSION, from: '0.9.6', ring: 'canary', hosts: 12, auto_rollback: true },
  },
  {
    actor: 'scheduler',
    action: 'exec',
    target: 'disk-space',
    payload: { schedule: 'disk-space', cron: '0 * * * *', targets: ONLINE.length, dispatched: ONLINE.length },
  },
  {
    actor: 'tanaka.misaki',
    action: 'schedule.update',
    target: 'inventory-apps',
    payload: { schedule: 'inventory-apps', cron: { from: '0 4 * * *', to: '0 3 * * *' }, enabled: true },
  },
  {
    actor: 'sato.kenji',
    action: 'notification.unack',
    target: 'ntf-0001',
    // A REAL host out of the fleet, not a plausible-looking string. An
    // audit row naming a PC the Agents page has never heard of is the kind
    // of detail that gives a demo away to anyone who checks.
    payload: { id: 'ntf-0001', pc_id: ONLINE[42]!.pc_id, reason: '誤操作の取り消し' },
  },
  {
    actor: 'tanaka.misaki',
    action: 'agent.prune',
    target: OFFLINE[0]!.pc_id,
    payload: { pc_id: OFFLINE[0]!.pc_id, last_heartbeat_days: 31, ttl_days: 30 },
  },
];

/**
 * A fixed pool, filtered — NOT generated to fit `limit`.
 *
 * The earlier version built exactly `limit` rows on the fly, which made the
 * table look right and every filter a no-op: narrowing the actor still
 * produced a full page, because the rows were minted after the query rather
 * than selected by it. Filters that never change the result are worse than
 * absent ones; they teach a viewer that the feature does nothing.
 */
const AUDIT_LOG = Array.from({ length: 240 }, (_, i) => {
  const e = AUDIT_EVENTS[i % AUDIT_EVENTS.length]!;
  return { id: 10_000 - i, ...e, occurred_at: iso((i + 1) * 173 * 1000) };
});

get(/^\/api\/audit$/, (_req, url) => {
  const q = (k: string) => url.searchParams.get(k)?.trim().toLowerCase() ?? '';
  const actor = q('actor');
  const action = q('action');
  const target = q('target');
  const payload = q('payload');
  const since = url.searchParams.get('since');
  const limit = Math.max(1, Math.min(500, Number(url.searchParams.get('limit') ?? '50') || 50));

  const rows = AUDIT_LOG.filter((e) => {
    if (actor && !e.actor.toLowerCase().includes(actor)) return false;
    if (action && !e.action.toLowerCase().includes(action)) return false;
    if (target && !e.target.toLowerCase().includes(target)) return false;
    // Substring over the serialised payload, which is what the backend's
    // own filter does — it is a text search into the JSON, not a key lookup.
    if (payload && !JSON.stringify(e.payload).toLowerCase().includes(payload)) return false;
    // ISO-8601 UTC strings sort lexicographically, but ONLY at equal width
    // and the same zone — the trap #1125 fixed elsewhere in this codebase.
    // Compare as instants instead.
    if (since && Date.parse(e.occurred_at) < Date.parse(since)) return false;
    return true;
  });

  return json(rows.slice(0, limit));
});

// ---- compliance ----

/**
 * Why a given check is unhappy on a given PC. The `detail` column is
 * the whole reason the Compliance page is worth looking at, so it gets
 * a real sentence — and one derived from THAT host, not a constant.
 * Sixteen rows all reading "空き容量 14% (残り 71 GB)" is the detail
 * that gives a demo away.
 */
function checkDetail(
  check: string,
  status: 'warn' | 'fail' | 'unknown',
  p: (typeof FLEET)[number],
  n: number,
): string | null {
  const freeGb = Math.round(p.disk_free_bytes / 1000 ** 3);
  const freePct = Math.round((p.disk_free_bytes / p.disk_total_bytes) * 100);
  switch (`${check}:${status}`) {
    case 'defender_realtime:fail':
      return 'リアルタイム保護が無効になっています';
    case 'bitlocker:fail':
      return 'C: が暗号化されていません';
    case 'bitlocker:warn':
      // NOT "not backed up to Entra ID / AD". kanade's customers are the
      // organisations that don't have that — an estate managed by Entra
      // or on-prem AD already has an escrow story and mostly doesn't
      // need this product. Phrasing the finding in terms of a directory
      // they don't run tells the wrong customer they're the audience.
      return '回復キーの保管が確認できません (端末内にのみ存在)';
    case 'bitlocker:unknown':
      return 'TPM の状態を取得できませんでした';
    case 'windows_update:fail':
      return `重要な更新プログラムが ${(n % 4) + 2} 件未適用 (最終適用: ${45 + n * 7} 日前)`;
    case 'windows_update:warn':
      return `再起動保留中の更新が ${(n % 3) + 1} 件あります`;
    case 'windows_update:unknown':
      return 'Windows Update サービスが停止しています';
    case 'screen_lock:warn':
      return `スクリーンセーバーの待ち時間が ${[30, 45, 60][n % 3]} 分に設定されています`;
    case 'edge_extensions:fail':
      return `許可リスト外の拡張機能が ${(n % 2) + 1} 件インストールされています`;
    case 'edge_extensions:warn':
      return '未評価の拡張機能が 1 件あります';
    case 'disk_free:fail':
    case 'disk_free:warn':
      return `空き容量 ${freePct}% (残り ${freeGb} GB)`;
    case 'os_eol:warn':
      return `${p.os_caption} ${p.os_version} は ${OS_EOL_DATE[p.os_version] ?? '-'} にサポート終了します`;
    case 'os_eol:fail':
      return `${p.os_caption} ${p.os_version} は ${OS_EOL_DATE[p.os_version] ?? '-'} にサポート終了済みです`;
    default:
      return null;
  }
}

get(/^\/api\/checks$/, () => {
  // Two checks can only flag hosts that actually qualify, or the
  // detail column contradicts the rest of the demo — a `disk_free`
  // warning on a box the Inventory page shows with 344 GB free, or an
  // EOL warning on Windows 11. Those draw from a filtered, ordered
  // pool; everything else walks the whole fleet with a stride coprime
  // to its length, so the attention lists aren't runs of consecutive
  // pc_ids and no single host collects every failure.
  const STRIDE = 37;
  // Windows hosts only. The generic pool used to be the whole fleet, and
  // every check that isn't `disk_free` is Windows-specific by name —
  // Defender, BitLocker, Windows Update, Windows Firewall, Edge policy.
  // The stride walk reaches the Linux tail (indices 240-247), so the
  // Compliance page could print "重要な更新プログラムが 3 件未適用" for a
  // host the Inventory page lists as Ubuntu. Same cross-page
  // contradiction the two filtered pools below already guard against.
  // 37 is coprime with 240 as well, so the rows still spread.
  const WINDOWS = FLEET.filter((p) => p.os_family === 'windows');
  // Both data-driven checks draw from the hosts their OWN classifier put
  // in that bucket — the same `diskBucket` / `osEolBucket` the tallies in
  // fleet.ts are computed from — so a row can never carry a status its
  // detail line contradicts. `disk_free` additionally sorts, so the fail
  // rows are the emptiest disks rather than an arbitrary three.
  const byBucket = (f: (p: (typeof FLEET)[number]) => string, status: string) =>
    FLEET.filter((p) => f(p) === status);
  const diskPool: Record<string, (typeof FLEET)[number][]> = {
    fail: byBucket(diskBucket, 'fail'),
    warn: byBucket(diskBucket, 'warn'),
  };
  for (const k of ['fail', 'warn']) {
    diskPool[k]!.sort(
      (a, b) => a.disk_free_bytes / a.disk_total_bytes - b.disk_free_bytes / b.disk_total_bytes,
    );
  }
  const eolPool: Record<string, (typeof FLEET)[number][]> = {
    fail: byBucket(osEolBucket, 'fail'),
    warn: byBucket(osEolBucket, 'warn'),
  };

  const rows: unknown[] = [];
  let cursor = 0;
  for (const c of CHECKS) {
    for (const status of ['fail', 'warn', 'unknown'] as const) {
      // Per status, NOT per check: an earlier version carried one cursor
      // across all three statuses of a check, so `os_eol` consumed seven
      // entries on its fail rows and then started its warn rows seven
      // hosts into the pool — running off the end of the expiring group
      // and listing releases supported until 2027 as warnings.
      const pool =
        c.name === 'disk_free' ? diskPool[status] : c.name === 'os_eol' ? eolPool[status] : null;
      for (let i = 0; i < c[status]; i++) {
        const p = pool && pool.length > 0
          ? pool[i % pool.length]!
          : WINDOWS[(cursor * STRIDE) % WINDOWS.length]!;
        cursor++;
        rows.push({
          pc_id: p.pc_id,
          check_name: c.name,
          label: c.label,
          status,
          detail: checkDetail(c.name, status, p, i),
          recorded_at: iso(((cursor % 11) + 1) * 37 * 60 * 1000),
        });
      }
    }
  }

  return json({
    counts: CHECKS.map((c) => ({
      check_name: c.name,
      label: c.label,
      ok: c.ok,
      warn: c.warn,
      fail: c.fail,
      unknown: c.unknown,
    })),
    rows,
    stale_days: 7,
    stale_attention: 0,
  });
});

// ---- schedules ----

get(/^\/api\/schedules\/upcoming$/, (_req, url) => {
  const limit = Number(url.searchParams.get('limit') ?? '6');
  return json(
    JOBS.slice(0, limit).map((j, i) => ({
      id: `sched-${j.id}`,
      job_id: j.id,
      when: `every ${j.every}`,
      next_run: isoIn((i * 17 + 4) * 60 * 1000),
    })),
  );
});

// ---- perf ----

get(/^\/api\/perf\/fleet$/, (_req, url) => {
  const metric = url.searchParams.get('metric') ?? 'cpu_pct';
  const mem = metric === 'mem_used_bytes';
  return json({
    metric,
    agg: url.searchParams.get('agg') ?? 'avg',
    from: iso(24 * 3600 * 1000),
    to: iso(0),
    step_seconds: 900,
    points: series(24, 15, (i, r) =>
      mem
        ? Math.round((7.4 + Math.sin(i / 7) * 1.1 + r() * 0.5) * 1024 ** 3)
        : Math.round((22 + Math.sin(i / 5) * 9 + r() * 6) * 10) / 10,
    ),
  });
});

get(/^\/api\/perf\/top$/, (_req, url) => {
  const metric = url.searchParams.get('metric') ?? 'cpu_pct';
  const limit = Number(url.searchParams.get('limit') ?? '5');
  const mem = metric === 'mem_used_bytes';
  const rows = [...FLEET]
    .sort((a, b) =>
      mem ? b.mem_used_bytes - a.mem_used_bytes : b.cpu_pct - a.cpu_pct,
    )
    .slice(0, limit)
    .map((p) => ({
      pc_id: p.pc_id,
      // The production /api/perf/* rows join `agents.hostname`, and the
      // SPA column is headed ホスト名 — sending the signed-in person's
      // name here put a human name under a machine column.
      hostname: p.hostname,
      value: mem ? p.mem_used_bytes : p.cpu_pct,
    }));
  return json({ metric, window_seconds: 300, rows });
});

get(/^\/api\/perf\/active-investigations$/, () =>
  json({
    window_seconds: 3600,
    rows: FLEET.filter((_, i) => i === 22 || i === 45).map((p) => ({
      pc_id: p.pc_id,
      hostname: p.hostname,
      latest_at: iso(6 * 60 * 1000),
    })),
  }),
);

// ---- analytics ----

/** App-usage minutes, the aggregate the Analytics page is built around. */
/**
 * Browsing mix for the `web-history` job's `web_visit` events, as
 * visit counts per domain.
 *
 * Shape note: the real `web-history` manifest lives in the private
 * ops-config repo, so this mirrors the contract the job schema
 * documents (`emit:` NDJSON → `obs_events` → an `aggregate:` widget
 * grouping on a `json_extract` path in the payload) rather than a
 * manifest checked in here. The payload key being grouped on is
 * `domain`.
 */
const WEB_VISITS: Array<[string, number]> = [
  ['portal.example.co.jp', 4820],
  ['outlook.office.com', 3960],
  ['teams.microsoft.com', 2740],
  ['www.google.com', 2510],
  ['docs.google.com', 1480],
  ['salesforce.com', 1120],
  ['www.yahoo.co.jp', 940],
  ['github.com', 610],
  ['qiita.com', 380],
  ['x.com', 210],
];

const APP_USAGE: Array<[string, number]> = [
  ['Microsoft Excel', 18420],
  ['Google Chrome', 16880],
  ['Microsoft Outlook', 14260],
  ['Microsoft Teams', 11930],
  ['Microsoft Word', 8640],
  ['Visual Studio Code', 6210],
  ['Microsoft PowerPoint', 5480],
  ['Adobe Acrobat', 3970],
  ['Slack', 2840],
  ['メモ帳', 1120],
];

get(/^\/api\/analytics$/, (_req, url) => {
  const pinned = url.searchParams.get('pinned') === 'true';
  const r = rng(0xa11a);

  // Per-PC scope. The Analytics page sends `pc_id` when the operator
  // flips the scope toggle to PC別 and expects widgets computed for that
  // host — ignoring the param (as this handler first did) left the page
  // showing the fleet rollup under a "PC別" toggle, which reads as the
  // toggle being broken.
  const pcId = url.searchParams.get('pc_id');
  if (pcId) {
    const p = findPc(pcId);
    if (!p) return json([]);
    const idx = FLEET.indexOf(p);

    // Honour the page's date pickers. Hardcoding a window here left the
    // swimlane hatched as "no data" outside a fixed 2 days no matter
    // what range the operator picked, and made the gauge below it
    // describe a different period than the strip above it.
    const toMs = Date.parse(url.searchParams.get('to') ?? '') || Date.now();
    const fromMs =
      Date.parse(url.searchParams.get('from') ?? '') || toMs - 2 * 86_400_000;
    const windowDays = Math.max(1, Math.ceil((toMs - fromMs) / 86_400_000));

    const events = eventsForPc(p, idx, windowDays + 1)
      .filter((e) => e.at >= fromMs && e.at <= toMs)
      .sort((a, b) => a.at - b.at);

    // Active minutes measured off the SAME events the strip paints, so
    // the gauge and the swimlane can't disagree. An unclosed span at the
    // window edge is clamped rather than dropped.
    let activeMinutes = 0;
    let openedAt: number | null = null;
    for (const e of events) {
      if (e.kind === 'active' && openedAt === null) openedAt = e.at;
      else if (e.kind === 'idle' && openedAt !== null) {
        activeMinutes += (e.at - openedAt) / 60_000;
        openedAt = null;
      }
    }
    if (openedAt !== null) activeMinutes += (Math.min(toMs, Date.now()) - openedAt) / 60_000;
    activeMinutes = Math.round(activeMinutes);

    const windowMinutes = Math.round((toMs - fromMs) / 60_000);
    const usageTotal = APP_USAGE.reduce((s, [, v]) => s + v, 0);

    return json([
      {
        dashboard: 'app-usage',
        title: `稼働状況 — ${p.pc_id}`,
        description: '電源 / セッション / スリープ / アクティブの再構成',
        scope: 'pc',
        render: 'op_timeline',
        from: new Date(fromMs).toISOString(),
        to: new Date(toMs).toISOString(),
        last_heartbeat: iso(p.heartbeat_ms_ago),
        events: events.map((e) => ({
          at: new Date(e.at).toISOString(),
          kind: e.kind,
          source: e.source,
        })),
      },
      {
        dashboard: 'app-usage',
        title: `アプリ利用時間 — ${p.last_logon_display_name}`,
        scope: 'pc',
        render: 'bar',
        // Split this host's measured active time across the app mix, so
        // the bar sums to the gauge's estimate instead of being a third
        // independent number on the same page.
        rows: APP_USAGE.map(([label, value]) => {
          const minutes = Math.round((value / usageTotal) * activeMinutes);
          return { label, value: minutes, est_minutes: minutes };
        }).filter((x) => x.value > 0),
      },
      {
        dashboard: 'web-history',
        title: `閲覧サイト — ${p.last_logon_display_name}`,
        description: 'ブラウザ履歴から収集した web_visit イベントのドメイン別集計',
        scope: 'pc',
        render: 'bar',
        // Scaled off the same measured active time as the app bar, so a
        // host that barely ran doesn't show a full week of browsing.
        rows: WEB_VISITS.map(([label, weight]) => {
          const visits = Math.round((weight / WEB_VISITS[0]![1]) * (activeMinutes / 9));
          return { label, value: visits };
        }).filter((x) => x.value > 0),
      },
      {
        dashboard: 'app-usage',
        title: '稼働率',
        scope: 'pc',
        render: 'gauge',
        total: windowMinutes,
        active: activeMinutes,
        ratio: windowMinutes > 0 ? activeMinutes / windowMinutes : 0,
        est_minutes: activeMinutes,
        // Bounds of what was actually observed — `first` must not be
        // later than `last`, which is what happens if you reach for the
        // heartbeat of an OFFLINE host as the upper bound.
        first: new Date(events[0]?.at ?? fromMs).toISOString(),
        last: new Date(events[events.length - 1]?.at ?? fromMs).toISOString(),
      },
    ]);
  }

  const usageBar = {
    dashboard: 'app-usage',
    title: 'アプリ利用時間 (フリート合計・24時間)',
    description: '各PCの前面ウィンドウ滞在時間を集計したもの',
    scope: 'fleet',
    pin_dashboard: true,
    render: 'bar',
    width: 'half',
    rows: APP_USAGE.map(([label, value]) => ({
      label,
      value,
      est_minutes: value,
    })),
  };

  // Counted from the fleet rather than hand-written, so this pie always
  // agrees with the OS column on the Inventory page. Two screenshots
  // that contradict each other is exactly the detail a viewer notices.
  const osCounts = new Map<string, number>();
  for (const p of FLEET) {
    // The Windows caption needs its release appended to be useful
    // ("Windows 11 Pro" alone doesn't distinguish 23H2 from 24H2);
    // the Linux caption already carries its version, so appending
    // would read "Ubuntu 24.04.1 LTS 24.04".
    const label =
      p.os_family === 'linux' ? p.os_caption : `${p.os_caption} ${p.os_version}`;
    osCounts.set(label, (osCounts.get(label) ?? 0) + 1);
  }
  const osPie = {
    dashboard: 'inventory',
    title: 'OS バージョン内訳',
    scope: 'fleet',
    pin_dashboard: true,
    render: 'pie',
    donut: true,
    rows: [...osCounts.entries()]
      .map(([label, value]) => ({ label, value }))
      .sort((a, b) => b.value - a.value),
  };

  // Counted from the fleet for the same reason the OS pie is.
  const siteCounts = new Map<string, number>();
  for (const p of FLEET) siteCounts.set(p.site, (siteCounts.get(p.site) ?? 0) + 1);
  const sitePie = {
    dashboard: 'inventory',
    title: '拠点別の台数',
    scope: 'fleet',
    pin_dashboard: true,
    render: 'pie',
    donut: true,
    rows: [...siteCounts.entries()]
      .map(([label, value]) => ({ label, value }))
      .sort((a, b) => b.value - a.value),
  };

  const webBar = {
    dashboard: 'web-history',
    title: '閲覧サイト (フリート合計・訪問回数)',
    description: 'ブラウザ履歴から収集した web_visit イベントのドメイン別集計',
    scope: 'fleet',
    pin_dashboard: true,
    render: 'bar',
    width: 'half',
    rows: WEB_VISITS.map(([label, value]) => ({ label, value })),
  };

  // Four pinned widgets: what the users do (apps / browsing), then what
  // the estate is (OS / sites). Deliberately a short list — a page that
  // pins everything defeats the point of pinning.
  //
  // Four pinned widgets: what the users do (apps / browsing), then what
  // the estate is (OS / sites). Deliberately a short list — a page that
  // pins everything defeats the point of pinning.
  //
  // The two bars carry `width: 'half'` (#1257) so each pairs with its
  // sibling instead of claiming the whole row. An SPA built before
  // #1257 ignores the field and stacks them, which is the old
  // behaviour — nothing here breaks on an older build.
  //
  // Rendering these two as donuts was tried instead, back when the
  // width was fixed by `render` alone, and rejected: a donut has to be
  // cut to ~6 slices before it reads at all, and its legend prints a
  // bare number, so "307h 20m" degrades to an unitless "307". The
  // ranked list with real durations IS the widget.
  if (pinned) return json([usageBar, webBar, osPie, sitePie]);

  return json([
    usageBar,
    osPie,
    sitePie,
    webBar,
    {
      dashboard: 'app-usage',
      title: '稼働率 (フリート平均)',
      scope: 'fleet',
      render: 'gauge',
      total: 248,
      active: 231,
      ratio: 231 / 248,
      est_minutes: 108_420,
      first: iso(24 * 3600 * 1000),
      last: iso(0),
    },
    {
      dashboard: 'app-usage',
      title: '時間帯別の稼働台数',
      description: '0〜23時のバケットごとに、稼働していたPCの割合',
      scope: 'fleet',
      render: 'timeline',
      metric: 'ratio',
      buckets: Array.from({ length: 24 }, (_, hour) => {
        // Office hours busy, a long lunch dip, near-silent overnight.
        const office = hour >= 8 && hour <= 19;
        const lunch = hour === 12;
        const base = office ? (lunch ? 0.55 : 0.86) : hour >= 20 && hour <= 22 ? 0.18 : 0.04;
        const active = Math.round(248 * (base + r() * 0.06));
        return { hour, total: 248, active };
      }),
    },
    {
      dashboard: 'inventory',
      title: 'ディスク空き容量が少ないPC',
      scope: 'fleet',
      render: 'table',
      columns: ['PC', '利用者', '空き容量', '空き率'],
      rows: [...FLEET]
        .sort((a, b) => a.disk_free_bytes / a.disk_total_bytes - b.disk_free_bytes / b.disk_total_bytes)
        .slice(0, 8)
        .map((p) => [
          p.pc_id,
          p.last_logon_display_name,
          `${Math.round(p.disk_free_bytes / 1000 ** 3)} GB`,
          `${Math.round((p.disk_free_bytes / p.disk_total_bytes) * 100)}%`,
        ]),
    },
  ]);
});

// ---- obs_events (the operational swimlane) ----

/**
 * A believable few days of office life for the first slice of the
 * fleet: boot before nine, sign in, work in bursts with idle gaps,
 * lunch, sign out in the evening, shut down. Weekends stay dark.
 *
 * The swimlane reconstructs its lanes from START/END kind pairs
 * (boot→shutdown, logon→logoff, sleep→resume, active→idle), so the
 * events have to come in matched pairs to paint anything. Generated
 * against the wall clock at request time, and truncated at "now" so
 * today's lane ends where today actually is rather than painting the
 * evening before it happened.
 */
// Tuned against the Events page's DEFAULT view (200-row limit, "2 days
// back to midnight"), not for maximum realism: on a WEEKDAY, 8 hosts at
// this density land ~150 rows in that window, so every lane is covered
// the moment the page opens. Raise either number and the page opens on
// its truncation warning with the left half of the strip hatched as
// "not fetched" — correct behaviour, terrible first impression.
//
// That tuning only holds on a weekday. Measured on a Sunday, the same
// default window returns 38 rows across 2 hosts, because it contains
// nothing but the weekend and the weekend runs the skeleton crew below.
// The lanes are then ~2.5 h wide and read as "uptime tracking is broken"
// rather than "nobody was in". Widening the window is the fix at the
// viewing end, not here — 7 days always contains a working day — which
// is what `demo/capture-screenshots.mjs` pins for the Events frame.
const EVENT_PC_COUNT = 8;
// Eight days, so a 7-day window is full rather than half grey. Day count
// costs nothing in the DEFAULT 2-day view — that filters by time, so only
// per-day event density decides its row count — but a window wider than
// this many days paints an empty left half, which reads as missing data
// rather than as the window simply reaching past the fixture.
const EVENT_DAYS = 8;

// `ObsEvent.source` is `<scheme>:<detail>`, not a bare word — see the
// field's doc comment in kanade-shared/src/wire/obs_event.rs. It exists
// so two collectors that share an `event_record_id` namespace stay
// distinguishable, so the detail half is load-bearing, not decoration.
const SRC_SYSTEM = 'winlog:System';
const SRC_SECURITY = 'winlog:Security';
const SRC_IDLE = 'agent:internal';
// The backend's heartbeat watchdog, not the agent — these two are the only
// kinds whose timestamp is an observation rather than a reconstruction, which
// is why they alone may close an unknown stretch (#1245).
const SRC_WATCHDOG = 'backend:heartbeat-watchdog';

type RawEvent = { pc_id: string; at: number; kind: string; source: string; payload: unknown };

/**
 * One PC's history. Seeded from the PC itself rather than from a walk
 * over the fleet, so a host's day is the same whether it's generated as
 * part of the Events page's 8-host list or on its own for the Analytics
 * page's per-PC swimlane. `idx` only decides weekend on-call duty.
 */
function eventsForPc(p: (typeof FLEET)[number], idx: number, days: number): RawEvent[] {
  const r = rng(0xe4e7 + idx * 7919);
  const ri = (min: number, max: number) => Math.floor(r() * (max - min + 1)) + min;
  const now = Date.now();
  const out: RawEvent[] = [];

  const midnight = new Date();
  midnight.setHours(0, 0, 0, 0);

  // Two personas, keyed off the model the Inventory page already shows
  // for this host: laptops SLEEP (lid down over lunch, in meetings, and
  // overnight instead of powering off), desktops shut down. Sleeping
  // only the odd host left the sleep lane empty on almost every strip,
  // which is both duller and less true — and deriving the split from
  // `model` means the swimlane agrees with the 機種 column rather than
  // being a second, independent invention.
  const isLaptop = /ThinkPad|Latitude|EliteBook|VAIO/.test(p.model);
  // One desktop loses power instead of shutting down — a tripped breaker, a
  // yanked cable, a hard reset. Windows logs NOTHING at the time: the only
  // trace is Kernel-Power 41, written during the NEXT boot and therefore
  // stamped with the morning's timestamp. Without a fixture for it the demo
  // only ever showed the clean case, and the strip painted every one of these
  // nights solid green in production while the suite stayed green too.
  //
  // …and on one of its mornings the recovery is missing too, because the
  // backend restarted (a deploy) while the host was away. The watchdog's
  // open-outage map does not survive that, so no `agent_online` is ever
  // written and the unknown stretch has no right edge to draw. What ends it
  // is the agent's own first sample of the morning — `agent:*`, so its
  // timestamp is an observation rather than a backfilled reconstruction.
  //
  // On the same host on purpose: an unclean power-off is the one night that
  // no OTHER record accounts for, so it is the only place the difference is
  // visible. On a night bounded by `shutdown`/`boot` or `sleep`/`resume` the
  // lanes settle the stretch themselves and an unclosed outage shows nothing.
  const losesPower = !isLaptop && idx === 4;
  const deployDuringOutage = losesPower ? 3 : -1;
  // Carries across days: a laptop that suspended last night is still
  // powered this morning, so it wakes with `resume` and never re-boots.
  // That is what puts a long amber block across Fri night → Mon morning.
  let powered = false;

  {
    for (let d = days - 1; d >= 0; d--) {
      const day0 = midnight.getTime() - d * 86_400_000;
      const dow = new Date(day0).getDay();
      const weekend = dow === 0 || dow === 6;
      // Weekends run a skeleton crew rather than going fully dark. A
      // dark weekend is the truthful shape, but it means anyone who
      // starts the demo on a Saturday opens the swimlane on an empty
      // page and concludes the product is broken. Three on-call hosts
      // keep today alive while the contrast with the weekday lanes
      // stays obvious.
      if (weekend && idx >= 3) continue;

      const push = (min: number, kind: string, source: string, payload: unknown = null) => {
        const at = day0 + min * 60_000;
        if (at > now) return false;
        out.push({ pc_id: p.pc_id, at, kind, source, payload });
        return true;
      };

      const startMin = weekend ? 10 * 60 + ri(0, 90) : 8 * 60 + ri(20, 55);
      const logon = startMin + ri(1, 3);
      const logoff = weekend ? startMin + ri(150, 300) : 18 * 60 + ri(-40, 70);

      const coldBoot = !powered;
      // The watchdog's recovery marker relative to the winlog. On a WAKE the
      // machine was already running, so heartbeats resume before the OS has
      // finished writing anything and the marker leads. On a COLD BOOT it
      // cannot: the agent has to be started by the machine it reports, so the
      // first beat necessarily follows the boot record. Putting it first
      // there made it the newest thing vouching for the host before the boot,
      // which collapsed the unaccounted stretch of an unclean power-off to
      // the two minutes between them and left the night reading as ON.
      const recovery = d < days - 1 && d !== deployDuringOutage;
      if (recovery && !coldBoot) push(startMin - ri(0, 2), 'agent_online', SRC_WATCHDOG);
      if (coldBoot) {
        if (!push(startMin, 'boot', SRC_SYSTEM, { uptime_reset: true })) break;
        powered = true;
        // The boot is also where an unclean power-off is discovered. It says
        // "the previous session did not end cleanly" and nothing about when
        // that was, which is why it is a marker on the strip rather than the
        // end of the night's power span.
        if (losesPower && d < days - 1) push(startMin + 1, 'unexpected_shutdown', SRC_SYSTEM);
        // …and only now can the backend hear it again.
        if (recovery) push(startMin + ri(1, 3), 'agent_online', SRC_WATCHDOG);
      } else if (!push(startMin, 'resume', SRC_SYSTEM)) {
        break;
      }
      push(logon, 'logon', SRC_SECURITY, {
        user: p.last_logon_user,
        logon_type: 2,
      });

      // Interactive bursts, broken up by the two suspends a laptop
      // actually takes during a day: lunch, and an afternoon away from
      // the desk (a meeting in another room, a move to a hot desk).
      const lunch = 12 * 60 + ri(0, 20);
      let t = logon + ri(1, 4);
      let sleptLunch = false;
      let sleptPm = false;
      let activeOpen = false;
      while (t < logoff) {
        const suspend =
          isLaptop &&
          ((!sleptLunch && t >= lunch && t < lunch + 45) ||
            (!sleptPm && t > 14 * 60 + 30 && t < 17 * 60 && r() < 0.4));
        if (suspend) {
          const forLunch = !sleptLunch && t >= lunch && t < lunch + 45;
          if (activeOpen) {
            if (!push(t, 'idle', SRC_IDLE)) break;
            activeOpen = false;
          }
          if (!push(t, 'sleep', SRC_SYSTEM)) break;
          t += forLunch ? ri(40, 65) : ri(25, 70);
          if (!push(t, 'resume', SRC_SYSTEM)) break;
          if (forLunch) sleptLunch = true;
          else sleptPm = true;
          continue;
        }
        const activeFor = ri(40, 95);
        if (!push(t, 'active', SRC_IDLE)) break;
        activeOpen = true;
        t += activeFor;
        if (t > logoff) break;
        if (!push(t, 'idle', SRC_IDLE)) break;
        activeOpen = false;
        t += ri(8, 24);
      }
      // The day's last burst is deliberately left OPEN. A real sampler
      // cannot close it: it debounces over five minutes, and the machine is
      // suspended ten seconds after logoff — there is no time to emit the
      // `idle`. This used to push one anyway, because an unclosed span ran
      // to the next morning's first idle and painted the night as work
      // (#1245). That is a product defect, not a property of laptops, and
      // with `agent_offline` now cutting the sampler lane the fixture can
      // stop covering for it. The span closes where the agent died, which is
      // the truth.

      push(logoff, 'logoff', SRC_SECURITY, { user: p.last_logon_user });

      // End of day. Laptops close the lid and suspend — including over
      // the weekend, which is where the strip gets its long amber block.
      //
      // Every night now. This used to stagger a real shutdown across hosts
      // every other night, because a laptop that only ever suspends has no
      // power event inside the default 2-day window, and `buildLanes` read
      // that as "this host has no winlog collector" — switching to the #970
      // idle-sampler backfill and painting power + session straight across
      // the night, claiming the user was signed in while the box was shut in
      // a bag. The stagger existed to keep the demo honest about a
      // product-side defect (#1256). With the predicate now answering the
      // question it was always meant to ask, the fixture can be what a
      // laptop fleet actually looks like.
      const wentAway = logoff + ri(1, 5);
      if (isLaptop) {
        push(wentAway, 'sleep', SRC_SYSTEM);
      } else if (losesPower) {
        // Off, with nothing logged to say so — see `losesPower` above.
        powered = false;
      } else {
        push(wentAway, 'shutdown', SRC_SYSTEM);
        powered = false;
      }
      // The agent dies with the host, and the backend notices a heartbeat or
      // two later. Without this the demo cannot show an outage at all — and
      // an outage is the only signal that says "nobody was watching", as
      // distinct from "nothing happened". The idle sampler debounces over
      // five minutes, so on a machine that suspends ten seconds after logoff
      // it never gets to emit the closing `idle`; that unclosed `active` span
      // running to the next morning is exactly #1245.
      push(wentAway + ri(1, 3), 'agent_offline', SRC_WATCHDOG);
    }
  }

  // A host whose heartbeats stopped two days ago cannot have reported
  // yesterday's logon. Without this, the swimlane shows an offline PC
  // booting and working normally *past* the last-heartbeat marker the
  // same strip draws — and the Agents page's "最終ハートビート" column
  // flatly contradicts the Analytics page. Truncating here is also what
  // makes the strip's hatched "asserted but unconfirmed" tail show up in
  // the demo at all.
  const lastBeat = Date.now() - p.heartbeat_ms_ago;
  const kept = out.filter((e) => e.at <= lastBeat);
  // …but the watchdog's own record is not the agent's to report. The backend
  // writes `agent_offline` precisely BECAUSE the heartbeats stopped, so the
  // cutoff above is the one event it does not bound. Stamp it at the cutoff
  // rather than keeping whatever the day loop scheduled for that evening: a
  // host that went quiet at 10:00 went quiet at 10:00, not at its usual
  // logoff. One record, not one per night it stayed away — the backend
  // notices the silence once.
  if (!p.online) {
    kept.push({
      pc_id: p.pc_id,
      at: lastBeat + 60_000,
      kind: 'agent_offline',
      source: SRC_WATCHDOG,
      payload: null,
    });
  }
  return kept;
}

function buildEvents(): RawEvent[] {
  return FLEET.slice(0, EVENT_PC_COUNT)
    .flatMap((p, idx) => eventsForPc(p, idx, EVENT_DAYS))
    .sort((a, b) => b.at - a.at);
}

const EVENT_KINDS = [
  'boot', 'shutdown', 'logon', 'logoff', 'sleep', 'resume', 'active', 'idle',
  'agent_offline', 'agent_online',
];
const EVENT_SOURCES = [SRC_SYSTEM, SRC_SECURITY, SRC_IDLE, SRC_WATCHDOG];

get(/^\/api\/obs_events$/, (_req, url) => {
  const limit = Number(url.searchParams.get('limit') ?? '500');
  const pcId = url.searchParams.get('pc_id');
  const from = url.searchParams.get('from');
  const to = url.searchParams.get('to');
  const kinds = (url.searchParams.get('kinds') ?? '').split(',').filter(Boolean);
  const sources = (url.searchParams.get('sources') ?? '').split(',').filter(Boolean);

  let rows = buildEvents();
  if (pcId) rows = rows.filter((e) => e.pc_id.toLowerCase().includes(pcId.toLowerCase()));
  if (from) rows = rows.filter((e) => e.at >= Date.parse(from));
  if (to) rows = rows.filter((e) => e.at <= Date.parse(to));
  if (kinds.length) rows = rows.filter((e) => kinds.includes(e.kind));
  if (sources.length) rows = rows.filter((e) => sources.includes(e.source));

  return json({
    events: rows.slice(0, limit).map((e, i) => ({
      id: rows.length - i,
      pc_id: e.pc_id,
      at: new Date(e.at).toISOString(),
      kind: e.kind,
      source: e.source,
      event_record_id: e.source.startsWith('winlog:') ? String(400_000 + i) : null,
      payload: e.payload,
    })),
  });
});

/**
 * Newest event before `before`, per PC and per lane — the same seeding the
 * real backend's `op_timeline` CTE has always done, now exposed for the
 * Events page too (#1256).
 *
 * The mock has to implement it: without seeds a host that did not reboot
 * inside the window reports no power event, the strip cannot tell that from
 * "this host has no winlog collector", and the demo reproduces the bug it is
 * supposed to demonstrate the fix for.
 */
const SEED_LANE_OF: Record<string, string> = {
  boot: 'power', shutdown: 'power', unexpected_shutdown: 'power',
  log_service_started: 'power', log_service_stopped: 'power',
  logon: 'session', logoff: 'session',
  sleep: 'sleep', resume: 'sleep',
  active: 'active', idle: 'active',
};

get(/^\/api\/obs_events\/lane_seeds$/, (_req, url) => {
  const pcs = (url.searchParams.get('pcs') ?? '').split(',').map((s) => s.trim()).filter(Boolean);
  const before = Date.parse(url.searchParams.get('before') ?? '');
  if (!pcs.length || Number.isNaN(before)) return json({ events: [] });
  const all = buildEvents();
  const out: RawEvent[] = [];
  for (const pc of pcs) {
    const newestPerLane = new Map<string, RawEvent>();
    for (const e of all) {
      if (e.pc_id !== pc || e.at >= before) continue;
      const lane = SEED_LANE_OF[e.kind];
      if (!lane) continue;
      const cur = newestPerLane.get(lane);
      if (!cur || e.at > cur.at) newestPerLane.set(lane, e);
    }
    out.push(...newestPerLane.values());
  }
  out.sort((a, b) => a.at - b.at);
  return json({
    events: out.map((e, i) => ({
      id: 900_000 + i,
      pc_id: e.pc_id,
      at: new Date(e.at).toISOString(),
      kind: e.kind,
      source: e.source,
      event_record_id: e.source.startsWith('winlog:') ? String(500_000 + i) : null,
      payload: e.payload,
    })),
  });
});

get(/^\/api\/obs_events\/kinds$/, () => json({ kinds: EVENT_KINDS }));
get(/^\/api\/obs_events\/sources$/, () => json({ sources: EVENT_SOURCES }));

// ---- inventory ----

const INV_BASIC_DISPLAY = [
  { field: 'hostname', label: 'ホスト名' },
  { field: 'os_caption', label: 'OS' },
  { field: 'os_version', label: 'バージョン' },
  { field: 'os_build', label: 'ビルド' },
  { field: 'model', label: '機種' },
  { field: 'serial', label: 'シリアル' },
  { field: 'cpu', label: 'CPU' },
  { field: 'mem_total_bytes', label: '搭載メモリ', type: 'bytes' as const },
  { field: 'disk_total_bytes', label: 'ディスク容量', type: 'bytes' as const },
  { field: 'disk_free_bytes', label: 'ディスク空き', type: 'bytes' as const },
  { field: 'last_boot', label: '最終起動', type: 'timestamp' as const },
];
const INV_BASIC_SUMMARY = INV_BASIC_DISPLAY.filter((f) =>
  ['hostname', 'os_caption', 'model', 'mem_total_bytes', 'disk_free_bytes'].includes(f.field),
);

const APP_CATALOG = [
  ['Microsoft 365 Apps for enterprise', '16.0.18324.20168', 'Microsoft Corporation'],
  ['Google Chrome', '138.0.7204.94', 'Google LLC'],
  ['Mozilla Firefox', '141.0', 'Mozilla'],
  ['Zoom Workplace', '6.5.7', 'Zoom Video Communications'],
  ['Adobe Acrobat Reader', '25.001.20531', 'Adobe Inc.'],
  ['7-Zip', '24.09', 'Igor Pavlov'],
  ['Visual Studio Code', '1.98.2', 'Microsoft Corporation'],
  ['Slack', '4.42.115', 'Slack Technologies'],
  ['Notepad++', '8.7.6', 'Notepad++ Team'],
  // NOTE: no `Java 8 Update 401` here — APP_HISTORY records it as
  // `removed` two weeks ago, so a host that still listed it under
  // インストール済みアプリ would contradict its own History tab. The
  // removal is the point of that history row (an EOL runtime taken off
  // the fleet); leaving the app installed makes it a lie.
];

const INV_APPS_DISPLAY = [
  {
    field: 'apps',
    label: 'インストール済みアプリ',
    type: 'table' as const,
    columns: [
      { field: 'name', label: '名称' },
      { field: 'version', label: 'バージョン' },
      { field: 'publisher', label: '発行元' },
      { field: 'install_date', label: 'インストール日', type: 'timestamp' as const },
    ],
  },
];

const INV_JOBS = [
  {
    manifest_id: 'inventory-basic',
    description: 'ハードウェア構成と OS バージョン',
    display: INV_BASIC_DISPLAY,
    summary: INV_BASIC_SUMMARY,
    pc_count: FLEET.length,
  },
  {
    manifest_id: 'inventory-apps',
    description: 'インストール済みアプリケーションの一覧',
    display: INV_APPS_DISPLAY,
    summary: null,
    pc_count: FLEET.length,
  },
];

get(/^\/api\/inventory\/jobs$/, () => json(INV_JOBS));

function invFacts(p: (typeof FLEET)[number], manifestId: string): Record<string, unknown> {
  if (manifestId === 'inventory-apps') {
    const r = rng(p.pc_id.length * 977 + Number(p.serial.slice(2, 6)));
    return {
      apps: APP_CATALOG.filter(() => r() > 0.25).map(([name, version, publisher]) => ({
        name,
        version,
        publisher,
        install_date: iso(Math.floor(r() * 540) * 86_400_000),
      })),
    };
  }
  return {
    hostname: p.hostname,
    os_caption: p.os_caption,
    os_version: p.os_version,
    os_build: p.os_build,
    model: p.model,
    serial: p.serial,
    cpu: p.os_family === 'linux' ? 'Intel Core i5-12500' : 'Intel Core i7-1355U',
    mem_total_bytes: p.mem_total_bytes,
    disk_total_bytes: p.disk_total_bytes,
    disk_free_bytes: p.disk_free_bytes,
    last_boot: iso(p.heartbeat_ms_ago + 9 * 3600 * 1000),
  };
}

get(/^\/api\/inventory\/by-job\/(.+)$/, (_req, url, m) => {
  const manifestId = decodeURIComponent(m[1]!);
  const job = INV_JOBS.find((j) => j.manifest_id === manifestId) ?? INV_JOBS[0]!;
  const limit = Number(url.searchParams.get('limit') ?? '50');
  const offset = Number(url.searchParams.get('offset') ?? '0');
  return json({
    manifest_id: job.manifest_id,
    display: job.display,
    summary: job.summary,
    total: FLEET.length,
    limit,
    offset,
    rows: FLEET.slice(offset, offset + limit).map((p) => ({
      pc_id: p.pc_id,
      facts: invFacts(p, job.manifest_id),
      collected_at: iso(p.heartbeat_ms_ago + 20 * 60 * 1000),
      last_logon_user: p.last_logon_user,
      last_logon_display_name: p.last_logon_display_name,
    })),
  });
});

// Per-PC facts, for the Inventory drill-down and the PC detail page.
// MUST stay below `/api/inventory/jobs` and `/api/inventory/by-job/…`
// above — first-registered-wins, and `([^/]+)` would swallow `jobs`.
get(/^\/api\/inventory\/([^/]+)$/, (_req, _url, m) => {
  const p = findPc(decodeURIComponent(m[1]!));
  if (!p) return json([]);
  return json(
    INV_JOBS.map((j) => ({
      job_id: j.manifest_id,
      facts: invFacts(p, j.manifest_id),
      display: j.display,
      summary: j.summary,
      collected_at: iso(p.heartbeat_ms_ago + 20 * 60 * 1000),
      recorded_at: iso(p.heartbeat_ms_ago + 19 * 60 * 1000),
    })),
  );
});

/**
 * Per-PC inventory history — when an app arrived, when it was upgraded,
 * when it was removed.
 *
 * This is the answer to "can you tell me when this got installed?", so
 * the fixture has to show all three change kinds and, for `changed`,
 * a before/after pair a reader can diff at a glance (the SPA renders
 * them side by side). `identity_json` carries the spec's primary key —
 * for `inventory_sw` that is the app name — which is what ties a
 * version bump to the row it happened on rather than to the array index.
 */
type AppChange = {
  daysAgo: number;
  app: string;
  kind: 'added' | 'removed' | 'changed';
  from?: string;
  to?: string;
  publisher: string;
};

const APP_HISTORY: AppChange[] = [
  { daysAgo: 2, app: 'Google Chrome', kind: 'changed', from: '137.0.7151.104', to: '138.0.7204.94', publisher: 'Google LLC' },
  { daysAgo: 3, app: 'Zoom Workplace', kind: 'changed', from: '6.5.4', to: '6.5.7', publisher: 'Zoom Video Communications' },
  { daysAgo: 6, app: 'Slack', kind: 'added', to: '4.42.115', publisher: 'Slack Technologies' },
  { daysAgo: 9, app: 'Microsoft 365 Apps for enterprise', kind: 'changed', from: '16.0.18227.20162', to: '16.0.18324.20168', publisher: 'Microsoft Corporation' },
  { daysAgo: 14, app: 'Java 8 Update 401', kind: 'removed', from: '8.0.4010.9', publisher: 'Oracle Corporation' },
  { daysAgo: 21, app: 'Adobe Acrobat Reader', kind: 'changed', from: '25.001.20428', to: '25.001.20531', publisher: 'Adobe Inc.' },
  { daysAgo: 28, app: 'Visual Studio Code', kind: 'changed', from: '1.97.2', to: '1.98.2', publisher: 'Microsoft Corporation' },
  { daysAgo: 35, app: '7-Zip', kind: 'added', to: '24.09', publisher: 'Igor Pavlov' },
  { daysAgo: 44, app: 'Mozilla Firefox', kind: 'changed', from: '140.0', to: '141.0', publisher: 'Mozilla' },
  { daysAgo: 61, app: 'Notepad++', kind: 'added', to: '8.7.6', publisher: 'Notepad++ Team' },
];

get(/^\/api\/inventory\/([^/]+)\/history\/pc\/([^/]+)$/, (_req, url, m) => {
  const manifestId = decodeURIComponent(m[1]!);
  const p = findPc(decodeURIComponent(m[2]!));
  if (!p) return json([]);
  // Only the software probe tracks history in this fixture — the
  // hardware one would need a RAM/disk swap to have anything to say.
  if (manifestId !== 'inventory-apps') return json([]);

  const sinceMs = Date.parse(url.searchParams.get('since') ?? '') || 0;
  // Offset per host so two PCs opened side by side don't show an
  // identical change log — a rollout reaches machines on different days.
  const skew = (FLEET.indexOf(p) % 5) * 0.7;

  return json(
    APP_HISTORY.map((c, i) => {
      const at = Date.now() - (c.daysAgo + skew) * 86_400_000;
      const row = (v?: string) =>
        v == null
          ? null
          : JSON.stringify({ name: c.app, version: v, publisher: c.publisher });
      return {
        id: 90_000 - i,
        pc_id: p.pc_id,
        job_id: manifestId,
        field_path: 'apps',
        identity_json: JSON.stringify({ name: c.app }),
        change_kind: c.kind,
        before_json: c.kind === 'added' ? null : row(c.from),
        after_json: c.kind === 'removed' ? null : row(c.to),
        observed_at: new Date(at).toISOString(),
      };
    }).filter((r) => Date.parse(r.observed_at) >= sinceMs),
  );
});

// ---- jobs / schedules / views ----

get(/^\/api\/jobs$/, () =>
  json(
    JOBS.map((j) => ({
      id: j.id,
      version: '1',
      description: j.label,
      execute: { shell: 'powershell', timeout: '5m', run_as: 'system' },
      inventory: j.id.startsWith('inventory-') ? {} : null,
      tags: j.id.startsWith('inventory-')
        ? ['inventory']
        : j.id === 'app-usage'
          ? ['analytics']
          : ['compliance'],
      live: { running: j.id === 'app-usage' ? 3 : 0, pending: 0 },
    })),
  ),
);

get(/^\/api\/schedules$/, () =>
  json(
    JOBS.map((j) => ({
      id: `sched-${j.id}`,
      when: { per_pc: { every: j.every } },
      job_id: j.id,
      target: { all: true, groups: [], pcs: [] },
      rollout: null,
      jitter: '2m',
      tz: 'local',
      starting_deadline: '30m',
      runs_on: 'agent',
      enabled: true,
      tags: ['standard'],
    })),
  ),
);

/**
 * Per-schedule coverage — the drawer behind a schedule row.
 *
 * Unlike the list endpoint this carries the `agents` roster, and the
 * page reads `.agents.filter(...)` straight off the response. Falling
 * through to the `[]` catch-all therefore crashed the whole page with
 * `Cannot read properties of undefined (reading 'filter')` — the same
 * shape of failure as the missing `/api/results/{id}` detail route, for
 * the same reason: a route that returns an object cannot borrow the
 * list-shaped fallback.
 */
get(/^\/api\/schedules\/([^/]+)\/coverage$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  const job = JOBS.find((j) => `sched-${j.id}` === id);
  if (!job) return new Response('schedule not found', { status: 404 });

  const r = rng(0xc0f1 + job.id.length * 31);
  const fail = Math.floor(r() * 4);
  const running = Math.floor(r() * 6);
  const pending = Math.floor(r() * 12);
  const ok = FLEET.length - fail - running - pending;

  // Ordered so the drawer's "not done yet" section leads with the rows
  // an operator actually has to look at.
  const states: Array<[number, 'fail' | 'running' | 'pending' | 'ok']> = [
    [fail, 'fail'],
    [running, 'running'],
    [pending, 'pending'],
    [ok, 'ok'],
  ];
  const agents: unknown[] = [];
  let i = 0;
  for (const [n, state] of states) {
    for (let k = 0; k < n; k++) {
      const p = FLEET[(i * 37) % FLEET.length]!;
      i++;
      agents.push({
        pc_id: p.pc_id,
        state,
        ...(state === 'ok' || state === 'fail'
          ? {
              version: '1',
              finished_at: iso(((i % 17) + 1) * 11 * 60 * 1000),
            }
          : {}),
      });
    }
  }

  return json({
    id,
    when: `every ${job.every}`,
    job_id: job.id,
    runs_on: 'agent',
    total: FLEET.length,
    ok,
    fail,
    running,
    pending,
    agents,
  });
});

get(/^\/api\/schedules\/coverage$/, () => {
  const r = rng(0xc0e5);
  return json(
    JOBS.map((j) => {
      const fail = Math.floor(r() * 4);
      const running = Math.floor(r() * 6);
      const pending = Math.floor(r() * 12);
      return {
        id: `sched-${j.id}`,
        total: FLEET.length,
        ok: FLEET.length - fail - running - pending,
        fail,
        running,
        pending,
      };
    }),
  );
});

get(/^\/api\/views$/, () =>
  json([
    {
      id: 'app-usage',
      description: 'アプリ利用時間のフリート集計',
      widgets: [
        { dashboard: 'app-usage', title: 'アプリ利用時間 (フリート合計・24時間)', render: 'bar', scope: 'fleet' },
        { dashboard: 'app-usage', title: '稼働率 (フリート平均)', render: 'gauge', scope: 'fleet' },
        { dashboard: 'app-usage', title: '時間帯別の稼働台数', render: 'timeline', scope: 'fleet' },
      ],
      tags: ['analytics'],
    },
    {
      id: 'inventory',
      description: 'インベントリのフリート内訳',
      widgets: [
        { dashboard: 'inventory', title: 'OS バージョン内訳', render: 'pie', scope: 'fleet' },
      ],
      sql_widgets: [{}],
      tags: ['inventory'],
    },
    {
      id: 'eol-exposure',
      description: 'サポート期限切れ OS / ソフトウェアの棚卸し',
      sql_widgets: [{}, {}],
      tags: ['security'],
    },
  ]),
);

// ---- YAML source (the Monaco editor) ----

/**
 * The manifest each demo job "was created from".
 *
 * Worth carrying even though nothing renders it as data: the first
 * question anyone asks about a job is *how is this defined?*, and the
 * answer is a YAML file, not a form. The SPA already has a Monaco
 * editor wired to `GET /api/{jobs,schedules,…}/{id}/yaml`; without a
 * fixture it opens empty and the demo has no answer to the question.
 *
 * These are written the way the real manifests are (see
 * `configs/jobs/*.yaml` and the private ops-config repo) rather than
 * minimised — the `inventory:` / `check:` / `emit:` hint that gives a
 * job its behaviour is the interesting part, and a stripped example
 * would hide exactly what a reader is looking for.
 */
const JOB_YAML: Record<string, string> = {
  'inventory-basic': `id: inventory-basic
version: 0.3.0
description: ハードウェア構成と OS バージョン

execute:
  shell: powershell
  timeout: 2m
  run_as: system
  script: |
    $os   = Get-CimInstance Win32_OperatingSystem
    $cs   = Get-CimInstance Win32_ComputerSystem
    $bios = Get-CimInstance Win32_BIOS
    $c    = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'"
    [pscustomobject]@{
      hostname         = $env:COMPUTERNAME
      os_caption       = $os.Caption
      os_version       = (Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion').DisplayVersion
      os_build         = "$($os.BuildNumber).$((Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion').UBR)"
      model            = $cs.Model
      serial           = $bios.SerialNumber
      cpu              = (Get-CimInstance Win32_Processor).Name
      mem_total_bytes  = $cs.TotalPhysicalMemory
      disk_total_bytes = $c.Size
      disk_free_bytes  = $c.FreeSpace
      last_boot        = $os.LastBootUpTime.ToString('o')
    } | ConvertTo-Json -Compress

# The projector upserts this object onto inventory_facts. \`display:\`
# names and orders the columns the SPA shows; \`summary:\` is the
# narrower set the fleet-wide list uses.
inventory:
  display:
    - { field: hostname,         label: ホスト名 }
    - { field: os_caption,       label: OS }
    - { field: os_version,       label: バージョン }
    - { field: os_build,         label: ビルド }
    - { field: model,            label: 機種 }
    - { field: serial,           label: シリアル }
    - { field: cpu,              label: CPU }
    - { field: mem_total_bytes,  label: 搭載メモリ,   type: bytes }
    - { field: disk_total_bytes, label: ディスク容量, type: bytes }
    - { field: disk_free_bytes,  label: ディスク空き, type: bytes }
    - { field: last_boot,        label: 最終起動,     type: timestamp }
  summary: [hostname, os_caption, model, mem_total_bytes, disk_free_bytes]
  # v0.35 / #93 — log changes to these scalars into inventory_history.
  track_history: [os_version, os_build, mem_total_bytes]
`,
  'inventory-apps': `id: inventory-apps
version: 0.4.0
description: インストール済みアプリケーションの一覧

execute:
  shell: powershell
  timeout: 5m
  run_as: system
  script: |
    $keys = @(
      'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*'
      'HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*'
    )
    $apps = Get-ItemProperty $keys -ErrorAction SilentlyContinue |
      Where-Object { $_.DisplayName } |
      ForEach-Object {
        [pscustomobject]@{
          name         = $_.DisplayName
          version      = $_.DisplayVersion
          publisher    = $_.Publisher
          install_date = $_.InstallDate
        }
      }
    # 進捗は stderr へ。stdout に混ざると projection が黙って死ぬ。
    [Console]::Error.WriteLine("collected $($apps.Count) apps")
    @{ apps = $apps } | ConvertTo-Json -Depth 4 -Compress

inventory:
  display:
    - field: apps
      label: インストール済みアプリ
      type: table
      columns:
        - { field: name,         label: 名称 }
        - { field: version,      label: バージョン }
        - { field: publisher,    label: 発行元 }
        - { field: install_date, label: インストール日, type: timestamp }
  # \`explode:\` turns the array into its own queryable table, and
  # track_history records per-row add / remove / version change — this
  # is what the History tab reads.
  explode:
    - field: apps
      table: inventory_sw
      primary_key: [name]
      track_history: true
`,
  'defender-status': `id: defender-status
version: 0.2.0
description: Defender 稼働確認

execute:
  shell: powershell
  timeout: 60s
  run_as: system
  script: |
    $s = Get-MpComputerStatus
    # AntimalwareEnabled は一部の Windows で null を返すため見ない。
    # 健全な端末を誤って fail にする。
    $ok = $s.RealTimeProtectionEnabled -and $s.AMRunningMode -in @('Normal','Passive')
    @{ check = @{
        name   = 'defender_realtime'
        status = if ($ok) { 'ok' } else { 'fail' }
        detail = if ($ok) { $null } else { 'リアルタイム保護が無効になっています' }
    } } | ConvertTo-Json -Compress

check:
  name: defender_realtime
  label: Defender リアルタイム保護
  status_field: check.status
  detail_field: check.detail
`,
  'app-usage': `id: app-usage
version: 0.2.0
description: アプリ利用時間の集計

execute:
  shell: powershell
  timeout: 60s
  run_as: user           # 前面ウィンドウは対話セッションでしか取れない
  script: |
    # 前回位置は watermark に記録し、新しい分だけを出す。
    ...

# NDJSON を 1 行 1 イベントとして obs_events に流す。成功時 stdout は
# ExecResult から落とされる（1台1日50行を実行結果表に二重保存しない）。
emit:
  type: events

# 読み取り専用の集計仕様。実行時には何も消費しないので他のヒントと共存する。
aggregate:
  - placement:
      analytics: app-usage
      dashboard: { pin: true, width: half }
    title: アプリ利用時間 (フリート合計・24時間)
    description: 各PCの前面ウィンドウ滞在時間を集計したもの
    scope: fleet
    kind: app_sample
    agg: count
    group_by: foreground.app
    sample_minutes: 2
    exclude: [LockApp]
    render: bar
`,
};

/**
 * The three demo views, each showing a different way to build one.
 *
 * A view is where an operator assembles a dashboard without touching
 * Rust, so the three are picked to span the range rather than to repeat
 * one shape: `aggregate:` over emitted events, `sql_widgets:` over the
 * projected tables, and a view that mixes both.
 */
const VIEW_YAML: Record<string, string> = {
  'app-usage': `# 端末の利用実態。app-usage ジョブが emit した obs_events を、
# 読み取り専用の集計仕様だけで組み立てる。SQL は書かない。
#
#   kanade view create configs/views/app-usage.yaml
id: app-usage
description: アプリ利用時間のフリート集計
tags: [analytics, attendance]

aggregate:
  # placement は Analytics タブと Dashboard のピン留めを 1 箇所で表す。
  # width は面ごとに指定できる（#1257）。
  - placement:
      analytics: app-usage
      dashboard: { pin: true, width: half }
    title: アプリ利用時間 (フリート合計・24時間)
    description: 各PCの前面ウィンドウ滞在時間を集計したもの
    scope: fleet
    kind: app_sample
    agg: count
    group_by: foreground.app
    sample_minutes: 2      # 1 サンプル = 2 分として時間に換算
    exclude: [LockApp]     # ロック画面が 1 位になるのを避ける
    limit: 10
    render: bar

  - placement: { analytics: app-usage }
    title: 稼働率 (フリート平均)
    scope: fleet
    kind: presence
    agg: ratio             # bool_path が true だった割合
    bool_path: active
    sample_minutes: 5
    render: gauge

  - placement: { analytics: app-usage }
    title: 時間帯別の稼働台数
    description: 0〜23時のバケットごとに、稼働していたPCの割合
    scope: fleet
    kind: presence
    agg: ratio
    bool_path: active
    time_bucket: hour      # timeline は time_bucket とセット
    render: timeline

  # 電源 / セッション / スリープ / アクティブの再構成。kind も agg も
  # 取らない特別枠で、複数種別のイベントから区間を組み立てる。
  - placement: { analytics: app-usage }
    title: 稼働状況
    scope: pc              # op_timeline は PC 単位のみ
    render: op_timeline
`,
  inventory: `# インベントリの内訳。集計 (obs_events) と SQL (投影テーブル) を
# 1 つの view に混在させられる。
id: inventory
description: インベントリのフリート内訳
tags: [inventory]

aggregate:
  - placement:
      analytics: inventory
      dashboard: { pin: true, width: half }
    title: OS バージョン内訳
    scope: fleet
    kind: inventory_snapshot
    agg: count
    group_by: os_version
    render: pie

sql_widgets:
  # inventory_facts は inventory-basic ジョブの JSON がそのまま入る。
  # 読み取り専用サンドボックスで実行される単一 SELECT。
  - title: ディスク空き容量が少ないPC
    description: 空き率の低い順
    query: |
      SELECT pc_id                                              AS "PC",
             json_extract(facts_json, '$.disk_free_bytes') / 1e9 AS "空き容量 (GB)",
             round(100.0 * json_extract(facts_json, '$.disk_free_bytes')
                         / json_extract(facts_json, '$.disk_total_bytes'), 1) AS "空き率 (%)"
      FROM inventory_facts
      WHERE job_id = 'inventory-basic'
      ORDER BY 3 ASC
      LIMIT 20
    refresh: 6h
    render: { kind: table }
    placement: { analytics: inventory }
`,
  'eol-exposure': `# サポート期限切れの棚卸し。「最新版である」ことと「安全である」
# ことは違う — 製品ラインが EOL なら、最新でも修正は二度と来ない。
#
# inventory_facts の OS と、feed-eol ジョブが取り込んだ
# endoflife.date のリリースサイクルを突き合わせる。
id: eol-exposure
description: "End-of-life exposure: host OS vs endoflife.date support cycles"
tags: [security, lifecycle]

sql_widgets:
  # フリート全体の見出し数字。ダッシュボードにピン留めする。
  - title: サポート終了済みの OS を使っている台数
    description: Windows のサポートサイクルを過ぎている PC
    query: |
      WITH os AS (
        SELECT pc_id,
               json_extract(facts_json, '$.os_caption') AS os_name,
               json_extract(facts_json, '$.os_version') AS disp
        FROM inventory_facts WHERE job_id = 'inventory-basic'
      )
      SELECT count(DISTINCT o.pc_id) AS pcs
      FROM os o
      JOIN feeds f
        ON f.feed_id = 'eol'
       AND lower(json_extract(f.data, '$.product')) = 'windows'
       AND lower(json_extract(f.data, '$.cycle')) LIKE '%' || lower(o.disp) || '%'
      WHERE date(json_extract(f.data, '$.eol')) <= date('now')
    refresh: 12h
    render: { kind: stat, value: pcs }
    placement: { analytics: Security, dashboard: { pin: true } }

  # :pc_id を bind すると PC 単位のウィジェットになる。Dashboard は
  # フリート スコープで PC を選ばないので、pin は付けられない。
  - title: この PC の OS サポート状況
    query: |
      SELECT json_extract(facts_json, '$.os_caption') AS "OS",
             json_extract(facts_json, '$.os_version') AS "リリース"
      FROM inventory_facts
      WHERE job_id = 'inventory-basic' AND pc_id = :pc_id
    refresh: 12h
    render: { kind: table }
    placement: { analytics: Security }
`,
};

const SCHEDULE_YAML = (id: string, jobId: string, every: string) => `id: ${id}
job_id: ${jobId}
when:
  per_pc:
    every: ${every}
target:
  all: true
jitter: 2m
tz: local
runs_on: agent
starting_deadline: 30m
enabled: true
tags: [standard]
`;

/** Fallback for jobs without a hand-written manifest above. */
function genericJobYaml(id: string): string {
  const job = JOBS.find((j) => j.id === id);
  return `id: ${id}
version: 0.1.0
description: ${job?.label ?? id}

execute:
  shell: powershell
  timeout: 5m
  run_as: system
  script: |
    # ...
    @{ check = @{ name = '${id.replace(/-/g, '_')}'; status = 'ok' } } | ConvertTo-Json -Compress

check:
  name: ${id.replace(/-/g, '_')}
  label: ${job?.label ?? id}
  status_field: check.status
  detail_field: check.detail
`;
}

const yaml = (body: string) =>
  new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });

get(/^\/api\/jobs\/([^/]+)\/yaml$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  if (!JOBS.some((j) => j.id === id)) return new Response('job not found', { status: 404 });
  return yaml(JOB_YAML[id] ?? genericJobYaml(id));
});

get(/^\/api\/views\/([^/]+)\/yaml$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  const body = VIEW_YAML[id];
  if (!body) return new Response('view not found', { status: 404 });
  return yaml(body);
});

get(/^\/api\/schedules\/([^/]+)\/yaml$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  const job = JOBS.find((j) => `sched-${j.id}` === id);
  if (!job) return new Response('schedule not found', { status: 404 });
  return yaml(SCHEDULE_YAML(id, job.id, job.every));
});

// ---- groups ----

/**
 * Group DEFINITIONS — the DYNAMIC groups, and only those.
 *
 * The split matters and I had it wrong: `/api/groups` is the membership
 * view over the `agent_groups` bucket, i.e. the groups an operator assigns
 * BY HAND (`情シス運用端末`, `サーバー`). Those are not defined by anything
 * and have no definition to show. A group DEFINITION is a YAML document
 * with a query in it — the backend re-evaluates it on `refresh` and writes
 * the result to `agent_groups_derived`, so membership follows the fleet
 * without anyone maintaining a roster.
 *
 * Which is the whole argument for the feature: stamp `site` and
 * `department` onto each agent once (`agent_meta`), and a PC that moves
 * office changes its metadata and lands in the right groups by itself.
 * Putting a hand-picked list in here would have shown the opposite.
 *
 * Sites and departments are generated from the same constants the fleet is,
 * so a definition cannot name a site no host has.
 */
type GroupDef = {
  id: string;
  description: string;
  /** A definition is DYNAMIC when it carries a query and STATIC when it
   *  carries a literal member list. Both are group definitions and both are
   *  authored as YAML — the page already labels them from exactly this
   *  field (`isDynamic()` in GroupDefs.tsx). An earlier version of this
   *  fixture only ever produced dynamic ones, which made the product look
   *  like it supported one shape when it supports two. */
  query?: string;
  refresh?: string;
  members?: string[];
  tags: string[];
};

const GROUP_DEFS: GroupDef[] = [
  ...SITES.map(([site]) => ({
    id: site,
    description: `${site} に設置された端末`,
    query: `meta.site == '${site}'`,
    refresh: '15m',
    tags: ['site'],
  })),
  ...DEPARTMENTS.map((dept) => ({
    id: dept,
    description: `${dept} の端末`,
    query: `meta.department == '${dept}'`,
    refresh: '15m',
    tags: ['department'],
  })),
  {
    id: '高負荷監視',
    description: 'CPU 使用率が高い状態が続いている端末。復旧すると自動的に外れる',
    query: 'cpu_pct > 60',
    refresh: '5m',
    tags: ['health', 'auto'],
  },
  // STATIC definitions: a literal list, authored as YAML like any other.
  // Neither of these is recoverable from a query over agent metadata —
  // "the machines the IT team works from" is not a property of a PC — and
  // that is exactly when you write the list down instead.
  {
    id: '情シス運用端末',
    description: '情報システム部門が運用作業に使う端末。所属部署では絞れないため個別に指定',
    members: FLEET.filter((p) => p.groups.includes('情シス運用端末')).map((p) => p.pc_id),
    tags: ['ops'],
  },
  {
    id: 'サーバー',
    description: '常時稼働させている機器。停止・再起動の扱いが一般端末と異なる',
    members: FLEET.filter((p) => p.groups.includes('サーバー')).map((p) => p.pc_id),
    tags: ['ops'],
  },
];

/** Resolved membership. Reads the fleet's own group list, which is stamped
 *  from the same site / department metadata the queries above select on —
 *  so the definition page and the membership page cannot disagree. */
function groupMembers(def: GroupDef): string[] {
  // A static definition IS its member list; a dynamic one is resolved
  // against the fleet, whose group stamps come from the same site /
  // department metadata the queries select on.
  if (!def.query) return [...(def.members ?? [])];
  return FLEET.filter((p) => p.groups.includes(def.id)).map((p) => p.pc_id);
}

get(/^\/api\/group-defs$/, () => json(GROUP_DEFS));

get(/^\/api\/group-defs\/([^/]+)\/members$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  const def = GROUP_DEFS.find((g) => g.id === id);
  if (!def) return new Response('group definition not found', { status: 404 });
  const members = groupMembers(def);
  return json({ id, kind: def.query ? 'dynamic' : 'static', count: members.length, members });
});

/**
 * The YAML behind one definition, for the Monaco editor.
 *
 * Generated from the definition rather than kept as a second copy: this is
 * what the operator authored, and a hand-written string next to the object
 * it is supposed to represent is a drift waiting to happen — edit the query
 * in one and the editor shows the other.
 */
get(/^\/api\/group-defs\/([^/]+)\/yaml$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  const g = GROUP_DEFS.find((x) => x.id === id);
  if (!g) return new Response('group definition not found', { status: 404 });
  // Built as lines and joined, not as one template literal: the body has
  // to contain real newlines and quoted YAML scalars, and hand-escaping
  // that inside a template string is how you end up shipping a file the
  // editor cannot parse.
  return yaml(
    (g.query
      ? [
          `id: ${g.id}`,
          `description: ${g.description}`,
          '',
          '# メンバーは query の評価結果。端末の meta が変われば自動で入れ替わる。',
          `query: ${JSON.stringify(g.query)}`,
          '',
          '# 再評価の間隔。site / department はそう頻繁には動かないので長め、',
          '# 状態で出入りするものは短く。',
          `refresh: ${g.refresh}`,
        ]
      : [
          `id: ${g.id}`,
          `description: ${g.description}`,
          '',
          '# query ではなく literal なメンバー一覧。端末の meta からは',
          '# 導けない所属なので、書き下すのが正しい。',
          'members:',
          ...(g.members ?? []).map((pc) => `  - ${pc}`),
        ]
    )
      .concat(['', `tags: [${g.tags.join(', ')}]`, ''])
      .join('\n'),
  );
});

/**
 * Per-group notification addresses (`GroupContacts`).
 *
 * #1279 deleted the separate Groups page and folded contacts into the
 * definitions page, so this is now read one group at a time rather than
 * arriving inside a fleet-wide overview. `/api/groups` itself is no longer
 * called by the SPA at all — the previous fixture served it, and serving an
 * endpoint nothing asks for is just a slower way to be wrong later.
 *
 * Only some groups have addresses. A demo where every group has a contact
 * hides the state the field exists for: the operator has to be able to see
 * at a glance which groups would silently drop an alert.
 */
/**
 * Operator accounts and permission groups (#1008).
 *
 * The roster is chosen so all THREE access states are on screen at once,
 * because they are the feature and they are easy to confuse:
 *
 *   allowed_features = null   unrestricted — every page
 *   allowed_features = [...]  a per-account allow-list
 *   permission_group = "..."  the group governs; the account's own list is
 *                             ignored, which is the case an operator misreads
 *
 * `sato.kenji` deliberately carries BOTH a stale `allowed_features` and a
 * permission group, because that is the shape the UI has to explain: the
 * list is still stored and no longer has any effect. A roster where the two
 * never co-occur would let a viewer believe they combine.
 *
 * Usernames match the actors in the audit log, so clicking through from an
 * audit row to "who is this" lands somewhere.
 */
const PERMISSION_GROUPS = [
  {
    name: 'ヘルプデスク',
    features: ['inventory', 'compliance', 'activity', 'notifications', 'remote'],
    created_at: iso(120 * 86_400_000),
    updated_at: iso(31 * 86_400_000),
  },
  {
    name: '監査担当',
    // Read-only by construction: audit and logs, nothing that dispatches.
    features: ['audit', 'logs', 'events', 'analytics'],
    created_at: iso(96 * 86_400_000),
    updated_at: iso(96 * 86_400_000),
  },
];

const ACCOUNTS = [
  {
    username: 'admin',
    role: 'admin',
    disabled: 0,
    must_change_pw: 0,
    email: 'admin@example.co.jp',
    allowed_features: null,
    permission_group: null,
    created_at: iso(400 * 86_400_000),
    updated_at: iso(400 * 86_400_000),
  },
  {
    username: 'tanaka.misaki',
    role: 'admin',
    disabled: 0,
    must_change_pw: 0,
    email: 'tanaka.misaki@example.co.jp',
    allowed_features: null,
    permission_group: null,
    created_at: iso(300 * 86_400_000),
    updated_at: iso(12 * 86_400_000),
  },
  {
    username: 'sato.kenji',
    role: 'operator',
    disabled: 0,
    must_change_pw: 0,
    email: 'sato.kenji@example.co.jp',
    // Stale, and ignored: `permission_group` wins (#1008 Phase 3).
    allowed_features: ['inventory', 'jobs', 'schedules'],
    permission_group: 'ヘルプデスク',
    created_at: iso(210 * 86_400_000),
    updated_at: iso(31 * 86_400_000),
  },
  {
    username: 'yamada.ryo',
    role: 'operator',
    disabled: 0,
    must_change_pw: 0,
    email: 'yamada.ryo@example.co.jp',
    allowed_features: null,
    permission_group: 'ヘルプデスク',
    created_at: iso(150 * 86_400_000),
    updated_at: iso(31 * 86_400_000),
  },
  {
    username: 'kobayashi.aoi',
    role: 'viewer',
    disabled: 0,
    must_change_pw: 0,
    email: 'kobayashi.aoi@example.co.jp',
    allowed_features: null,
    permission_group: '監査担当',
    created_at: iso(96 * 86_400_000),
    updated_at: iso(96 * 86_400_000),
  },
  {
    username: 'ito.natsuki',
    role: 'viewer',
    disabled: 0,
    // Invited and has not signed in yet — the state the setup-link flow
    // exists for, and one a roster of settled accounts would never show.
    must_change_pw: 1,
    email: 'ito.natsuki@example.co.jp',
    allowed_features: ['inventory', 'compliance'],
    permission_group: null,
    created_at: iso(2 * 86_400_000),
    updated_at: iso(2 * 86_400_000),
  },
  {
    username: 'nakamura.taro',
    role: 'operator',
    // Left the team; kept rather than deleted so the audit trail still
    // resolves to a name.
    disabled: 1,
    must_change_pw: 0,
    email: null,
    allowed_features: null,
    permission_group: null,
    created_at: iso(500 * 86_400_000),
    updated_at: iso(45 * 86_400_000),
  },
];

get(/^\/api\/accounts$/, () => json(ACCOUNTS));

/** Member counts are COUNTED from the roster, not stored beside it — the
 *  two are exactly the pair that drifts. */
get(/^\/api\/permission-groups$/, () =>
  json(
    PERMISSION_GROUPS.map((g) => ({
      ...g,
      member_count: ACCOUNTS.filter((a) => a.permission_group === g.name).length,
    })),
  ),
);

/**
 * Fleet-wide group membership.
 *
 * #1279 deleted the Groups page, but this endpoint did NOT go with it —
 * `GroupPicker` still reads it to offer group targets on the Run and Jobs
 * screens. I removed it while adapting to that release, on the assumption
 * that a deleted page meant a dead route, and the picker went empty. The
 * lesson is cheap to state and easy to forget: a page going away does not
 * retire its endpoints, and the only way to know is to grep for callers.
 */
get(/^\/api\/groups$/, () =>
  json({
    groups: (() => {
      const byName = new Map<string, string[]>();
      for (const p of FLEET) {
        for (const g of p.groups) byName.set(g, [...(byName.get(g) ?? []), p.pc_id]);
      }
      return [...byName.entries()]
        .map(([name, members]) => ({
          name,
          members,
          has_config: false,
          emails: GROUP_EMAILS[name] ?? [],
        }))
        .sort((a, b) => b.members.length - a.members.length);
    })(),
  }),
);

const GROUP_EMAILS: Record<string, string[]> = {
  高負荷監視: ['ops@example.co.jp'],
  サーバー: ['ops@example.co.jp', 'infra@example.co.jp'],
  情シス運用端末: ['jyoho-sys@example.co.jp'],
  東京本社: ['sysadmin@example.co.jp'],
};

get(/^\/api\/groups\/([^/]+)\/email$/, (_req, _url, m) =>
  json({ emails: GROUP_EMAILS[decodeURIComponent(m[1]!)] ?? [] }),
);

// ---- notifications ----

/** Line break for the Markdown bodies below — the arrays read better
 *  than one long string with escapes, and this keeps the join honest. */
const BR = '\n';

/**
 * The sent notifications, shared by the list and the detail route so a
 * row and the page behind it can't disagree.
 */
const NOTIFICATIONS = [
  {
    id: 'ntf-0003',
    priority: 'warn',
    require_ack: true,
    title: '【重要】月次セキュリティ更新の適用について',
    // Markdown, rendered through `marked` + DOMPurify against the
    // allowlist in `lib/markdown.ts` — strong / em / lists / tables /
    // links / blockquote / code, and headings since #1262.
    //
    // These bodies are the demo's argument that a fleet-wide notice is a
    // DOCUMENT, not a paragraph: a lead, then steps, then who to contact.
    // They use real `##` headings rather than the bold lead-ins an
    // earlier draft used — that was a workaround for the allowlist, and
    // leaving it in place would have meant shipping promo material that
    // quietly declined to show a feature this repo had just added.
    //
    // Kept byte-identical to `crates/kanade-client/web/demo/fixtures.ts`.
    // The operator sends these and the end user receives them, so the two
    // demos are two views of ONE notice; different wording between them
    // is not a cosmetic slip, it is the screenshots contradicting each
    // other.
    body: [
      '**本日 18:00 以降**、Windows Update の適用と再起動をお願いします。',
      '',
      '作業中のファイルは必ず保存してください。再起動は自動では行われません。',
      '',
      '## 手順',
      '',
      '1. スタート → 設定 → Windows Update',
      '2. 「更新プログラムのチェック」を実行',
      '3. 表示された更新をすべて適用',
      '4. 求められたら再起動',
      '',
      '## 拠点ごとの推奨時間帯',
      '',
      '| 拠点 | 推奨時間帯 |',
      '| --- | --- |',
      '| 東京本社 | 18:00 - 20:00 |',
      '| 大阪支社 | 18:30 - 20:30 |',
      '| その他拠点 | 業務終了後いつでも |',
      '',
      '> 再起動後に不具合が出た場合は、**適用を取り消さず**に情報システム部へご連絡ください。',
      '',
      '手順の詳細は [社内ポータルの手順書](https://portal.example.co.jp/it/windows-update) を参照してください。',
    ].join(BR),
    toast: true,
    issued_ms_ago: 3 * 3600 * 1000,
    issued_by: 'jyoho-sys',
    expires_in_ms: 2 * 86_400_000,
    /** Fraction of the audience that has confirmed. */
    acked: 0.72,
  },
  {
    id: 'ntf-0002',
    priority: 'info',
    require_ack: false,
    title: '社内 Wi-Fi メンテナンスのお知らせ',
    body: [
      '今週土曜 **22:00 〜 翌 2:00** に無線 LAN のメンテナンスを実施します。',
      '',
      '対象は以下の SSID です。時間中は接続が断続的に切れます。',
      '',
      '- `KANADE-CORP` — 全拠点',
      '- `KANADE-GUEST` — 東京本社のみ',
      '',
      '有線 LAN と VPN は影響を受けません。**作業予定のある方は有線をご利用ください。**',
      '',
      '進捗は [ステータスページ](https://portal.example.co.jp/it/status) で随時更新します。',
    ].join(BR),
    toast: false,
    issued_ms_ago: 28 * 3600 * 1000,
    issued_by: 'jyoho-sys',
    expires_in_ms: null,
    acked: 0,
  },
  {
    // The one the Client App demo receives live, a few seconds in
    // (`fixtures.ts::PUSHED_NOTIFICATION`). It is here because an
    // operator sent it — a notice that arrives on an end user's screen
    // with no record on the sending side would be the two demos
    // disagreeing about what happened.
    id: 'ntf-0004',
    priority: 'info',
    require_ack: false,
    title: 'ヘルプデスク受付時間の変更',
    body: [
      '来週より、ヘルプデスクの受付時間を **9:00 〜 18:00** に変更します。',
      '',
      '時間外の連絡は [問い合わせフォーム](https://portal.example.co.jp/it/contact) をご利用ください。',
    ].join(BR),
    toast: true,
    issued_ms_ago: 5 * 60 * 1000,
    issued_by: 'jyoho-sys',
    expires_in_ms: null,
    acked: 0,
  },
  {
    id: 'ntf-0001',
    priority: 'emergency',
    require_ack: true,
    title: '不審メールにご注意ください',
    body: [
      '請求書を装った**添付ファイル付きメール**が複数届いています。',
      '',
      '**開かないでください。** 添付を開くと端末が暗号化される恐れがあります。',
      '',
      '## 見分け方',
      '',
      '- 差出人が取引先に似ているが、ドメインが 1 文字違う',
      '- 件名に「請求書」「支払い」「至急」を含む',
      '- 添付が `.zip` または `.iso`',
      '',
      '## 該当メールを受け取ったら',
      '',
      '1. 開かない・返信しない',
      '2. `security@example.co.jp` へ**添付したまま転送**',
      '3. 転送後、元のメールを削除',
      '',
      '> すでに開いてしまった場合は、**端末をネットワークから切断**したうえで内線 1234 までご連絡ください。',
    ].join(BR),
    toast: true,
    issued_ms_ago: 4 * 86_400_000,
    issued_by: 'jyoho-sys',
    expires_in_ms: null,
    acked: 0.94,
  },
] as const;

function notificationRecord(n: (typeof NOTIFICATIONS)[number]) {
  return {
    id: n.id,
    priority: n.priority,
    require_ack: n.require_ack,
    title: n.title,
    body: n.body,
    toast: n.toast,
    issued_at: iso(n.issued_ms_ago),
    issued_by: n.issued_by,
    expires_at: n.expires_in_ms == null ? null : isoIn(n.expires_in_ms),
  };
}

/**
 * The backend's own signing key (#1260).
 *
 * Without it the Agents page still renders the column, but abstains from
 * every comparison — `signingState()` takes `backend` as optional and
 * deliberately declines to guess. So `staleRing` and `wrongKey`, the two
 * states an operator has to act on, would never appear no matter what the
 * agents report. The demo has to serve this for the column to mean anything.
 */
get(/^\/api\/command-signing$/, () => json(BACKEND_SIGNING_KEY));

get(/^\/api\/notifications$/, () => json(NOTIFICATIONS.map(notificationRecord)));

/**
 * One notification's content plus who has confirmed it.
 *
 * `audience` is the per-PC roster the page renders as the confirmation
 * list, and it reads `notification.expires_at` straight off the
 * response — so falling through to the `[]` catch-all crashed the page
 * with `Cannot read properties of undefined (reading 'expires_at')`.
 * Third route in this file to hit that; see the sweep note in
 * README.md.
 */
get(/^\/api\/notifications\/([^/]+)$/, (_req, _url, m) => {
  const id = decodeURIComponent(m[1]!);
  const n = NOTIFICATIONS.find((x) => x.id === id);
  if (!n) return new Response('notification not found', { status: 404 });

  // A notification goes to the whole fleet here; the roster is the
  // online hosts, which is what an operator would actually reach.
  const targets = ONLINE.slice(0, 60);
  const ackCount = Math.round(targets.length * n.acked);
  const audience = targets.map((p, i) => {
    const confirmed = n.require_ack && i < ackCount;
    // One PC took its confirmation back — the unack path exists and a
    // demo that never shows it hides half of the feature.
    const retracted = n.require_ack && i === ackCount;
    return {
      pc_id: p.pc_id,
      last_logon_user: p.last_logon_user,
      last_logon_display_name: p.last_logon_display_name,
      confirmed,
      ...(confirmed || retracted
        ? { acked_at: iso(n.issued_ms_ago - (i + 1) * 60_000) }
        : {}),
      ...(retracted ? { unacked_at: iso(n.issued_ms_ago - (i + 1) * 30_000) } : {}),
    };
  });

  return json({
    notification: notificationRecord(n),
    acks: audience
      .filter((a) => a.acked_at)
      .map((a) => ({
        pc_id: a.pc_id,
        user_sid: 'S-1-5-21-1004336348-1177238915-682003330-1013',
        acked_at: a.acked_at!,
        account: a.last_logon_user,
        ...(a.unacked_at ? { unacked_at: a.unacked_at } : {}),
      })),
    audience,
    target: { all: true, groups: [], pcs: [] },
  });
});


// ---- rollout ----

get(/^\/api\/agents\/releases$/, () =>
  json(
    [CURRENT_VERSION, '0.9.6', '0.9.3'].map((version, i) => ({
      version,
      size: 14_820_000 - i * 12_000,
      digest: `sha256:${'0123456789abcdef'.repeat(4)}`.slice(0, 71),
      modified: iso((i * 9 + 2) * 86_400_000),
    })),
  ),
);

// ---- collect ----

get(/^\/api\/collect\/bundles$/, () => {
  const r = rng(0xb0d1);
  return json(
    FLEET.slice(0, 12).map((p, i) => {
      // The object-store key carries the collection date, so it has to
      // ride the SAME offset as `collected_at` below — a literal date
      // here would still read 2026-07-30 a year from now, next to a
      // "2h ago" timestamp on the same row.
      const collectedMsAgo = (i + 1) * 5400 * 1000;
      const day = iso(collectedMsAgo).slice(0, 10);
      return {
        key: `event-logs/${p.pc_id}/${day}.zip`,
        pc_id: p.pc_id,
        job_id: 'collect-event-logs',
        collected_at: iso(collectedMsAgo),
        label: null,
        size: Math.round(2_400_000 + r() * 18_000_000),
        digest: 'sha256:9f2c…',
        name: 'イベントログ一式',
        description: 'System / Application ログの直近 7 日分',
      };
    }),
  );
});

// ---- jetstream ----

/**
 * The bootstrap contract, copied from `kanade_shared::kv`.
 *
 * These lists are `ALL_STREAMS` / `ALL_KV_BUCKETS` / `ALL_OBJECT_STORES`,
 * which is the same set the real `/api/jetstream/status` probes and the same
 * set the health card counts. The demo had four streams, two of which
 * (`COMMANDS`, `OBS`) do not exist in this product at all — invented names
 * that a viewer who knows the system would spot immediately, and a count
 * that could never match the health card's.
 *
 * If a stream is added to `kv.rs` this list has to follow; nothing enforces
 * that from here, which is why the names are spelled out rather than
 * abbreviated — a diff against `kv.rs` is then a plain text comparison.
 */
const JS_STREAMS = [
  'INVENTORY',
  'RESULTS',
  'EXEC',
  'EVENTS',
  'AUDIT',
  'OBS_EVENTS',
  'NOTIFICATIONS',
] as const;

const JS_BUCKETS = [
  'script_current',
  'script_status',
  'agents_state',
  'agent_config',
  'agent_groups',
  'agent_groups_derived',
  'agent_meta',
  'group_contacts',
  'schedules',
  'jobs',
  'fleet_config',
  'notifications_read',
  'jobs_yaml',
  'schedules_yaml',
] as const;

const JS_STORES = [
  'agent_releases',
  'app_packages',
  'scripts',
  'result_output',
  'collections',
] as const;

/** What the fleet-health card reports as "healthy / total". Derived, so it
 *  cannot drift from the list the JetStream page renders. */
const JETSTREAM_RESOURCE_COUNT = JS_STREAMS.length + JS_BUCKETS.length + JS_STORES.length;

get(/^\/api\/jetstream\/status$/, () => {
  const r = rng(0x1a57);
  const probe = (name: string, bytes: number, max?: number) => ({
    name,
    exists: true,
    bytes: Math.round(bytes),
    ...(max ? { max_bytes: max } : {}),
    messages: Math.round(bytes / 900),
  });
  // Sizes scale with what each resource actually carries for 248 hosts, so
  // the page reads as a fleet rather than as a list of equal rows.
  const streamBytes: Record<string, number> = {
    EVENTS: 412 * 1024 ** 2,
    RESULTS: 268 * 1024 ** 2,
    INVENTORY: 196 * 1024 ** 2,
    OBS_EVENTS: 96 * 1024 ** 2,
    EXEC: 18 * 1024 ** 2,
    AUDIT: 7 * 1024 ** 2,
    NOTIFICATIONS: 1.4 * 1024 ** 2,
  };
  const storeBytes: Record<string, number> = {
    result_output: 1.2 * 1024 ** 3,
    collections: 640 * 1024 ** 2,
    agent_releases: 210 * 1024 ** 2,
    app_packages: 88 * 1024 ** 2,
    scripts: 3 * 1024 ** 2,
  };
  return json({
    streams: JS_STREAMS.map((n) => probe(n, streamBytes[n] ?? 1024 ** 2, 2 * 1024 ** 3)),
    kv_buckets: JS_BUCKETS.map((n) => probe(`KV_${n}`, 48 * 1024 + r() * 3 * 1024 ** 2)),
    object_stores: JS_STORES.map((n) => probe(`OBJ_${n}`, storeBytes[n] ?? 1024 ** 2, 4 * 1024 ** 3)),
  });
});

// ---- JSON Schemas (Monaco's completion + validation) ----

/**
 * Served from the checked-in `docs/schemas/*.json` rather than
 * hand-written here.
 *
 * The real backend generates these from the Rust types at run time
 * (`schemars::schema_for!`), and the repo keeps a copy in sync via a
 * test. Reading that copy means the demo editor offers exactly the
 * fields the current build accepts — a hand-maintained stub would drift
 * and start red-squiggling valid YAML, which is worse than no schema at
 * all when the whole point is to show what a job looks like.
 */
const SCHEMA_FILES: Record<string, string> = {
  'manifest.json': 'job.schema.json',
  'schedule.json': 'schedule.schema.json',
  'view.json': 'view.schema.json',
  'group-def.json': 'group-def.schema.json',
};

get(/^\/api\/schemas\/([^/]+)$/, async (_req, _url, m) => {
  const file = SCHEMA_FILES[decodeURIComponent(m[1]!)];
  if (!file) return new Response('unknown schema', { status: 404 });
  // demo/ sits at crates/kanade-backend/web/demo → four levels up.
  const path = new URL(`../../../../docs/schemas/${file}`, import.meta.url);
  try {
    return json(await Bun.file(path).json());
  } catch {
    // Running the mock from a copy without the repo around it is a
    // legitimate thing to do; Monaco just loses completion.
    console.log(`  [demo-api] schema ${file} not readable — editor loses completion`);
    return json({});
  }
});

// ---------------------------------------------------------------- serve

const unimplemented = new Set<string>();

/**
 * Literal routes win over parameterised ones, whatever order they were
 * registered in.
 *
 * Without this the dispatcher is first-registered-wins, and
 * `/api/agents/([^/]+)` silently answers `404 agent not found` for
 * `/api/agents/releases` — a literal route defined 1,300 lines further
 * down, in the section it belongs to. The `[]` catch-all can't save it
 * either, because a route *did* match. That shipped: the Rollout page
 * lost its release list and nothing said so.
 *
 * A comment saying "keep the literals above" is what failed the first
 * time, so this makes the invariant mechanical instead: a pattern is
 * "parameterised" if it captures, and every non-capturing pattern is
 * tried first. Ordering within each group still follows registration,
 * which is all the remaining overlaps need (`([^/]+)$` and
 * `([^/]+)/meta$` anchor differently and can't collide).
 */
const isParameterised = (p: RegExp) => p.source.includes('(');
const SORTED_ROUTES = [
  ...ROUTES.filter(([, p]) => !isParameterised(p)),
  ...ROUTES.filter(([, p]) => isParameterised(p)),
];


// ---- remote view (#1140) ----

/**
 * The operator-side remote screen is the one page that is not HTTP: the SPA
 * opens `ws(s)://…/api/remote/<pc_id>/ws` and reads length-prefixed binary
 * frames. Without a socket here the page renders its chrome over a black
 * canvas forever, which is worse than not showing the feature at all.
 *
 * Frame layout, mirroring `kanade_shared::wire::remote` and the decoder in
 * `src/lib/remoteFrame.ts`:
 *
 *     [u32 LE meta length][meta JSON][payload bytes]
 *
 * The desktop served is a SYNTHETIC image (`demo/remote-desktop.jpg`), drawn
 * with generic window shapes and invented data — no vendor chrome, no logo,
 * and above all not a picture of anyone's actual machine. Regenerate it with
 * `demo/make-remote-desktop.sh`, which reproduces the committed file exactly.
 */
const REMOTE_DESKTOP = await Bun.file(new URL('./remote-desktop.jpg', import.meta.url)).arrayBuffer();
const REMOTE_W = 1920;
const REMOTE_H = 1080;

function remoteFrame(meta: unknown, payload: ArrayBuffer | null = null): Uint8Array {
  const metaBytes = new TextEncoder().encode(JSON.stringify(meta));
  const body = payload ? new Uint8Array(payload) : new Uint8Array(0);
  const out = new Uint8Array(4 + metaBytes.length + body.length);
  new DataView(out.buffer).setUint32(0, metaBytes.length, /* littleEndian */ true);
  out.set(metaBytes, 4);
  out.set(body, 4 + metaBytes.length);
  return out;
}

type RemoteSocketData = { pcId: string; seq: number; timer?: ReturnType<typeof setInterval> };

const server = Bun.serve<RemoteSocketData>({
  port: PORT,
  // Loopback-only: a Caddy reverse proxy is the intended public surface
  // when this runs outside `cargo make demo` (e.g. a hosted demo box).
  // Matches the backend's own `127.0.0.1` bind in deploy/linux/setup.sh —
  // don't rely on the cloud firewall alone.
  hostname: process.env.DEMO_API_HOST ?? '127.0.0.1',
  idleTimeout: 60,
  // Return type annotated: `fetch` calls `server.upgrade`, so without it TS
  // recurses through `server`'s own initialiser and gives up (TS7022/7023).
  fetch(req): Response | Promise<Response> | undefined {
    const url = new URL(req.url);

    // Upgrade before the route table: this path has no HTTP handler, and a
    // failed upgrade must not fall through to the `[]` catch-all.
    const remote = url.pathname.match(/^\/api\/remote\/([^/]+)\/ws$/);
    if (remote) {
      // Annotated because `server` is still being initialised at this point,
      // so TS cannot infer the return type without recursing into itself.
      const ok: boolean = server.upgrade(req, {
        // Echo the viewer's subprotocol back; the browser aborts the
        // handshake if the server picks one it did not offer.
        headers: { 'Sec-WebSocket-Protocol': 'kanade.remote.v1' },
        data: { pcId: decodeURIComponent(remote[1]!), seq: 0 } satisfies RemoteSocketData,
      });
      return ok ? undefined : new Response('expected a websocket upgrade', { status: 426 });
    }

    for (const [method, pattern, handler] of SORTED_ROUTES) {
      if (req.method !== method) continue;
      const m = url.pathname.match(pattern);
      if (m) return handler(req, url, m);
    }

    if (url.pathname === '/health') return new Response('ok');

    // Log each unknown route once — a page that needs more than the
    // demo data covers should be discoverable from the terminal, not
    // by reading a stack trace in devtools.
    const key = `${req.method} ${url.pathname}`;
    if (!unimplemented.has(key)) {
      unimplemented.add(key);
      console.log(`  [demo-api] no fixture for ${key} — returning []`);
    }
    // `[]` rather than an empty body: `apiFetch` turns an empty body
    // into `undefined`, and TanStack Query treats a queryFn that
    // resolves `undefined` as a failure — so a missing fixture would
    // paint the SPA's red "couldn't load" banner instead of an empty
    // card. `[]` also survives both consumer shapes: pages that do
    // `data ?? []` map over nothing, and pages that reach for
    // `data.rows ?? []` read undefined off it and fall back the same way.
    return json([]);
  },

  websocket: {
    open(ws) {
      const d = ws.data;
      ws.send(
        remoteFrame({
          kind: 'started',
          session_id: `demo-${d.pcId}`,
          // Null on purpose: the real agent answers Start before its capture
          // child has taken a frame, so a viewer must size from the first
          // tile. Sending real geometry here would let a viewer bug that
          // depends on this pass in the demo and fail on a live host.
          screen_w: null,
          screen_h: null,
          allow_input: false,
        }),
      );
      const tile = () => {
        d.seq += 1;
        ws.send(
          remoteFrame(
            {
              kind: 'tile',
              frame_seq: d.seq,
              tile_index: 0,
              tile_count: 1,
              x: 0,
              y: 0,
              w: REMOTE_W,
              h: REMOTE_H,
              screen_w: REMOTE_W,
              screen_h: REMOTE_H,
              captured_at_ms: Date.now(),
              encoding: 'jpeg',
            },
            REMOTE_DESKTOP,
          ),
        );
      };
      tile();
      // A real session only sends what changed, so a still desktop goes
      // quiet. Repeating the full frame keeps the viewer's tile counter
      // moving, which is what tells an operator the session is alive.
      d.timer = setInterval(tile, 2000);
    },
    close(ws) {
      const d = ws.data;
      if (d.timer) clearInterval(d.timer);
    },
    message() {
      // The viewer sends input events when `allow_input` is set. It is not.
    },
  },
});

/**
 * Shelf-life check for the one fixture that is tied to the real calendar.
 *
 * Almost everything here is an OFFSET from `Date.now()`, so it stays true
 * whenever the demo is run. `OS_EOL_DATE` cannot be: those are real Windows
 * servicing dates, and rewriting them to keep a shape would put fabricated
 * facts in front of an audience that may well know the real ones.
 *
 * So they age instead. The intended picture is "one build expired, one
 * approaching, the current one fine" — and roughly three months from the day
 * this was written, the approaching one expires and the warning band empties
 * into the failure band. Later still, every Windows host is flagged.
 *
 * Finding that out mid-demo is the bad outcome. Finding it out in the
 * terminal, before anyone is watching, is a cheap one.
 */
{
  const buckets = { ok: 0, warn: 0, fail: 0 };
  for (const p of FLEET) buckets[osEolBucket(p)]++;
  const windowsHosts = FLEET.filter((p) => p.os_family === 'windows').length;
  if (buckets.warn === 0 || buckets.fail === 0 || buckets.warn + buckets.fail > windowsHosts / 2) {
    console.warn(
      '[demo-api] OS_EOL_DATE has aged: ' +
        `ok=${buckets.ok} warn=${buckets.warn} fail=${buckets.fail}. ` +
        'The Compliance page no longer shows the intended mix (one build ' +
        'expired, one approaching). Refresh the dates in demo/fleet.ts ' +
        'before showing this to anyone.',
    );
  }
}

console.log(`[demo-api] listening on http://localhost:${server.port}`);
console.log(`[demo-api] ${FLEET.length} fake PCs (${ONLINE.length} online, ${OFFLINE.length} offline)`);
