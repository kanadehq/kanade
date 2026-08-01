/**
 * Demo stand-in for `@tauri-apps/api/core` (`cargo make demo-client`).
 *
 * The Client App's WebView reaches the outside world through exactly two
 * doors: `invoke()` here and `listen()` in `tauri-event.ts`. There is no
 * HTTP layer, so the SPA demo's trick — repointing Vite's `/api` proxy —
 * has no equivalent. Aliasing the two Tauri modules is the same idea
 * applied one level down: `vite.config.ts` swaps them in demo mode, the
 * app is untouched and unaware, and nothing demo-shaped ships in the
 * product bundle.
 *
 * The state below is deliberately mutable. A demo where 確認 does
 * nothing, or where a job button prints a canned block instantly, shows
 * the screens but not the product — the interesting part of this app is
 * that pressing something changes what you see next.
 */

import {
  PRODUCT_VERSION,
  CHECKS,
  DISPLAY_NAME,
  iso,
  isoIn,
  JOBS,
  JOB_OUTPUT,
  NOTIFICATIONS,
  PC_ID,
  SUPPORT_CODE,
} from './fixtures';
import { emit } from './tauri-event';

type Notification = {
  id: string;
  priority: string;
  require_ack: boolean;
  title: string;
  body: string;
  toast: boolean;
  issued_at: string;
  issued_by?: string | null;
  expires_at?: string | null;
  acked_at?: string | null;
};

/** Live notification list — ack/unack mutate this, and a pushed notice
 *  prepends to it, so the panel reflects what the user just did. */
const notifications: Notification[] = NOTIFICATIONS.map((n) => ({
  id: n.id,
  priority: n.priority,
  require_ack: n.require_ack,
  title: n.title,
  body: n.body,
  toast: n.toast,
  issued_at: iso(n.issued_ms_ago),
  issued_by: n.issued_by,
  expires_at: n.expires_in_ms == null ? null : isoIn(n.expires_in_ms),
  acked_at: n.acked_ms_ago == null ? null : iso(n.acked_ms_ago),
}));

export function addNotification(n: Omit<Notification, 'issued_at' | 'acked_at'>): Notification {
  const full: Notification = { ...n, issued_at: iso(0), acked_at: null };
  notifications.unshift(full);
  return full;
}

/** Live support grants. Empty until the passcode is entered. */
let grants: Array<{ scope: string; label?: string | null; expires_at: string }> = [];

const SUPPORT_GRANT_MS = 30 * 60 * 1000;

let runSeq = 0;

/**
 * Stream a job's output as `klp-notification` progress pushes, the same
 * shape the agent sends. Runs on real timers so the panel shows a run
 * actually progressing rather than a finished block appearing at once —
 * "you can watch it work" is the whole reason this screen exists.
 */
/**
 * Timers still pending per run, so a kill can cancel them.
 *
 * Without this, `jobs_kill` emitted `killed` and the already-scheduled
 * callbacks kept firing behind it — the panel showed the run stop, then
 * resume printing lines, then report `completed`. The cancel button
 * appeared not to work, which is the one thing this screen has to make
 * believable: a job you can watch is only reassuring if you can also
 * stop it.
 */
const runTimers = new Map<string, ReturnType<typeof setTimeout>[]>();

function startRun(jobId: string, runId: string): void {
  const script = JOB_OUTPUT[jobId] ?? [[500, '完了しました。']];
  const progress = (params: Record<string, unknown>) =>
    emit('klp-notification', {
      jsonrpc: '2.0',
      method: 'jobs.progress',
      params: { run_id: runId, ...params },
    });

  const timers: ReturnType<typeof setTimeout>[] = [];
  runTimers.set(runId, timers);

  progress({ status: 'running' });
  for (const [delay, line] of script) {
    timers.push(
      setTimeout(() => progress({ status: 'running', stdout_chunk: `${line}\n` }), delay),
    );
  }
  const last = script[script.length - 1]?.[0] ?? 0;
  timers.push(
    setTimeout(() => {
      progress({ status: 'completed', exit_code: 0 });
      runTimers.delete(runId);
    }, last + 600),
  );
}

/** Cancel whatever `startRun` still has queued for this run. */
function stopRun(runId: string): void {
  for (const t of runTimers.get(runId) ?? []) clearTimeout(t);
  runTimers.delete(runId);
}

export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  // A touch of latency everywhere: an app whose every action lands in
  // zero milliseconds photographs fine but demos badly — the spinners
  // and disabled states never appear, so a viewer never sees that the
  // app handles them.
  await new Promise((r) => setTimeout(r, 60));

  switch (cmd) {
    case 'get_handshake':
      return { client_display_name: DISPLAY_NAME } as T;

    case 'app_version':
      return PRODUCT_VERSION as T;

    case 'state_snapshot':
      return {
        pc_id: PC_ID,
        online: true,
        checks: CHECKS,
        agent_version: PRODUCT_VERSION,
        target_version: PRODUCT_VERSION,
      } as T;

    case 'jobs_list': {
      // The agent applies the unlock gate when it builds this list, so a
      // gated row is absent until a grant is live — not present-but-
      // disabled. Mirroring that is the difference between demoing the
      // feature and demoing a greyed-out button.
      const unlocked = new Set(grants.map((g) => g.scope));
      return { items: JOBS.filter((j) => !j.unlock || unlocked.has(j.unlock)) } as T;
    }

    case 'jobs_execute': {
      const jobId = String(args?.id ?? '');
      const runId = `run-${++runSeq}-${jobId}`;
      startRun(jobId, runId);
      return { run_id: runId } as T;
    }

    case 'jobs_kill': {
      const runId = String(args?.runId ?? '');
      // Cancel BEFORE announcing, or the queued callbacks race the
      // `killed` update and overwrite it.
      stopRun(runId);
      emit('klp-notification', {
        jsonrpc: '2.0',
        method: 'jobs.progress',
        params: { run_id: runId, status: 'killed' },
      });
      return undefined as T;
    }

    case 'notifications_list':
      return { items: notifications, next_cursor: null } as T;

    case 'notifications_ack': {
      const n = notifications.find((x) => x.id === String(args?.id ?? ''));
      const acked_at = iso(0);
      if (n) n.acked_at = acked_at;
      return { acked_at } as T;
    }

    case 'notifications_unack': {
      const n = notifications.find((x) => x.id === String(args?.id ?? ''));
      if (n) n.acked_at = null;
      return { unacked_at: iso(0) } as T;
    }

    case 'notifications_subscribe':
      return undefined as T;

    case 'support_status':
      return { grants } as T;

    case 'support_unlock': {
      if (String(args?.code ?? '') !== SUPPORT_CODE) {
        // Reject the way the agent does, so the demo shows the error
        // path too — a passcode field that always succeeds teaches the
        // viewer nothing about what a wrong code looks like.
        throw new Error('コードが正しくありません');
      }
      grants = [
        { scope: 'support', label: 'サポート作業', expires_at: isoIn(SUPPORT_GRANT_MS) },
      ];
      return { grants } as T;
    }

    case 'support_lock': {
      const released = grants.length;
      grants = [];
      return { released } as T;
    }

    case 'open_external_url': {
      const url = String(args?.url ?? '');
      if (/^https?:\/\//i.test(url)) window.open(url, '_blank', 'noopener');
      return undefined as T;
    }

    /**
     * NOT a no-op, and this one is load-bearing under
     * `demo-client-app`.
     *
     * The main window is configured `visible: false`
     * (`tauri.conf.json`) and is revealed only when the frontend asks
     * for it — the Rust command shows and focuses it. Stubbing this
     * out left the app running with its window hidden forever: the
     * process was up, Vite was serving, and nothing appeared on
     * screen.
     *
     * `@tauri-apps/api/window` is deliberately NOT aliased, so inside
     * the real shell we can reach the genuine window handle. In the
     * browser demo there is no Tauri, so the import is skipped and
     * this stays the no-op it has to be there.
     */
    /**
     * A no-op, and it has to be — the window is already on screen.
     *
     * The product reveals its window (configured `visible: false`) by
     * invoking a Rust command, because the frontend cannot do it
     * itself: `core:default` grants only READ access to the window
     * API (`allow-title`, `allow-is-visible`, …) and no
     * `allow-show` / `allow-set-focus`. An earlier draft of this shim
     * called `getCurrentWindow().show()` from JS, which cannot work
     * twice over — that call routes back through
     * `@tauri-apps/api/core`, which is aliased to this very file, so it
     * lands in the `default:` branch below and returns null.
     *
     * So the app-window demo makes the window visible from the start
     * via `tauri.demo.conf.json` instead, and this stays the no-op it
     * already had to be in the browser.
     */
    case 'show_main_window':
      return undefined as T;

    /**
     * Forwarded to the real Rust command, which is the only way to get
     * the toast the product actually ships: a raw WinRT toast with
     * `scenario=reminder`, so it stays on screen until dismissed, keeps
     * a 確認 button, and remains in the Action Center. The plugin's
     * `sendNotification` is a plain Web Notification — no buttons, gone
     * in ~7s — and confirming straight from the toast is one of the
     * things worth showing.
     *
     * REQUIRES the demo protocol registration. The native toast embeds
     * `launch="kanade-client://<id>"` (activationType=protocol, #647),
     * and that scheme is registered machine-wide to the INSTALLED
     * client:
     *
     *   HKLM\Software\Classes\kanade-client\shell\open\command
     *     = "C:\Program Files\Kanade\kanade-client.exe" "%1"
     *
     * Left alone, clicking the demo's toast opens the real client in
     * front of whoever is being shown the product — observed, not
     * theorised. `scripts/demo-client-protocol.ps1` points the scheme
     * at this build under HKCU, which wins over HKLM for the current
     * user, and `cargo make demo-client-app` removes it again on exit.
     *
     * Still a no-op in the browser demo, where there is no shell.
     */
    case 'show_emergency_toast': {
      if ('__TAURI_INTERNALS__' in window) {
        return (
          window as unknown as {
            __TAURI_INTERNALS__: { invoke: (c: string, a?: unknown) => Promise<T> };
          }
        ).__TAURI_INTERNALS__.invoke(cmd, args);
      }
      return undefined as T;
    }

    /**
     * Forwarded in the shell: this is the COLD-launch half of the toast
     * click. When the app is not already running, the protocol
     * activation starts it and the notification id arrives as launch
     * state rather than as an event, so a stub returning null loses the
     * focus on exactly the path a real user takes first.
     *
     * `null` in the browser, where there is no launch to inspect.
     */
    case 'get_launch_focus':
    case 'get_launch_notification': {
      if ('__TAURI_INTERNALS__' in window) {
        return (
          window as unknown as {
            __TAURI_INTERNALS__: { invoke: (c: string, a?: unknown) => Promise<T> };
          }
        ).__TAURI_INTERNALS__.invoke(cmd, args);
      }
      return null as T;
    }

    default:
      console.warn(`[demo-client] unmocked invoke("${cmd}") — returning null`);
      return null as T;
  }
}
