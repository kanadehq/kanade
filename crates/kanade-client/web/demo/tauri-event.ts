/**
 * Demo stand-in for `@tauri-apps/api/event` (`cargo make demo-client`).
 *
 * The app subscribes to six KLP events; this is the other half of the
 * shim described in `tauri-core.ts`. Beyond satisfying the import, it is
 * what lets the demo show the thing a screenshot can't: a notification
 * arriving on its own while the window is open, which is the moment the
 * whole notification feature exists for.
 */

import { PUSHED_NOTIFICATION, PUSH_DELAY_MS } from './fixtures';

type Handler = (event: { event: string; payload: unknown }) => void;

const handlers = new Map<string, Set<Handler>>();

/** True inside the Tauri shell, false in the browser demo. */
const inShell = (): boolean => '__TAURI_INTERNALS__' in window;

type Internals = {
  invoke: (cmd: string, args?: unknown) => Promise<unknown>;
  transformCallback: (cb: (p: unknown) => void) => number;
};
const internals = (): Internals =>
  (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

/**
 * Rust-side events this shim lets through, and nothing else.
 *
 * An allowlist rather than a passthrough, because the app window may
 * well be running on a machine with a real agent on the other end of
 * the pipe — this repo's own dev box is exactly that. A blanket
 * forward would let a live `klp-notification` from the real agent land
 * in a panel that is otherwise pure fiction, i.e. put a real notice
 * from a real fleet into a promo screenshot. One event is needed and
 * one event is forwarded.
 *
 * The three that matter all carry an activation, never fleet data:
 *
 *   klp-focus-notification  a `kanade-client://show?id=<id>` protocol
 *                           launch — what a click on the toast BODY or
 *                           its 確認 button actually does (#647)
 *   klp-show-notification   the `--show-notification <id>` argv path
 *   klp-resurface           `--resurface`, a plain bring-to-front
 *
 * Getting this list wrong is silent, and I got it wrong first time by
 * grepping for the emit nearest the word "notification" instead of
 * reading the activation branch: `app.rs` emits FOCUS_NOTIFICATION on
 * the protocol path and SHOW_NOTIFICATION only on the argv path, so the
 * one event a toast click produces was the one not forwarded. Nothing
 * logs a dropped event — the toast opened the window and the panel
 * simply stayed where it was.
 */
const FORWARDED_FROM_RUST = new Set([
  'klp-focus-notification',
  'klp-show-notification',
  'klp-resurface',
]);

/** Deliver an event to whoever subscribed. Used by the shim itself. */
export function emit(name: string, payload: unknown): void {
  for (const h of handlers.get(name) ?? []) h({ event: name, payload });
}

/** Subscribe to the genuine Tauri event system, bypassing this alias. */
async function listenForReal(name: string, handler: Handler): Promise<() => void> {
  const api = internals();
  const eventId = (await api.invoke('plugin:event|listen', {
    event: name,
    target: { kind: 'Any' },
    handler: api.transformCallback((payload) => handler(payload as Parameters<Handler>[0])),
  })) as number;
  return () => {
    void api.invoke('plugin:event|unlisten', { event: name, eventId });
  };
}

export async function listen(name: string, handler: Handler): Promise<() => void> {
  let set = handlers.get(name);
  if (!set) handlers.set(name, (set = new Set()));
  set.add(handler);

  // `klp-connected` is delivered to each subscriber AS IT SUBSCRIBES,
  // rather than broadcast once from module scope.
  //
  // The app treats this event as "the pipe is up, pull everything", so
  // missing it means the panel sits on 接続待ち / 読み込み中 forever. A
  // module-scope `queueMicrotask(() => emit(...))` raced the app's own
  // registration: the app calls `listen()` from an async init, so
  // whether the handler existed yet depended on how many microtasks the
  // engine had already drained. It won in Chrome and lost in WebView2 —
  // the browser demo looked fine while the real window hung, which is
  // the worst way for a race to present.
  //
  // Emitting on subscribe cannot lose the event: there is exactly one
  // subscriber to satisfy and it is registering right now.
  if (name === 'klp-connected') {
    queueMicrotask(() => handler({ event: name, payload: null }));
  }

  // Aliasing this module replaced `listen` outright, so events emitted
  // from RUST reached nobody — the app subscribes through us and we
  // were the only thing feeding it. That is invisible until something
  // outside the WebView tries to talk to the app, and then it looks
  // like a UI bug: clicking the toast's 確認 button opened the window
  // but never moved it to the notice, because `klp-show-notification`
  // was emitted, dropped, and never missed by anything that logs.
  let stopReal: (() => void) | undefined;
  if (inShell() && FORWARDED_FROM_RUST.has(name)) {
    try {
      stopReal = await listenForReal(name, handler);
    } catch (err) {
      console.warn(`[demo-client] real listen("${name}") failed`, err);
    }
  }

  return () => {
    set!.delete(handler);
    stopReal?.();
  };
}

// …then push a notice a few seconds later, as if an operator had just
// sent it.
//
// The method name is load-bearing: the app's `klp-notification` listener
// switches on the exact wire string, and `notifications.new` is what the
// agent sends (`kanade_shared::ipc::method::NOTIFICATIONS_NEW`). An
// earlier draft used a plausible-sounding `notifications.push`, which no
// branch matched, so the event was silently dropped — and the demo still
// *looked* right, because `klp-show-notification` below makes
// `surfaceForwardedToast` re-fetch `notifications_list` and the panel
// repopulated from the mutated fixture. The headline scenario worked by
// accident, down a path a real push never takes. Exactly the kind of
// almost-right fixture this whole demo exists to avoid shipping.
setTimeout(async () => {
  const { addNotification } = await import('./tauri-core');
  const n = addNotification(PUSHED_NOTIFICATION);
  emit('klp-notification', {
    jsonrpc: '2.0',
    method: 'notifications.new',
    params: n,
  });

  // Browser only. There, nothing can be clicked — the OS toast does not
  // exist — so the demo stands in for the click to show where a notice
  // takes you. In the app window the real toast is on screen and the
  // real click path is wired, and faking it there made the window jump
  // to the front the instant the toast appeared, which reads as the app
  // barging in rather than notifying.
  if (!inShell()) emit('klp-show-notification', n.id);
}, PUSH_DELAY_MS);
