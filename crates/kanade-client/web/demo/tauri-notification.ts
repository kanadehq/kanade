/**
 * Demo stand-in for `@tauri-apps/plugin-notification`.
 *
 * Aliased for two reasons. The obvious one: OS toasts don't exist in a
 * browser tab, and the real module would reach for a Tauri plugin that
 * isn't there.
 *
 * The one that actually forced it: the plugin's own bundle imports
 * `addPluginListener` from `@tauri-apps/api/core`, which the core alias
 * had already redirected to our shim — so the build failed on a missing
 * export from a module the app never touches directly. Shimming the
 * plugin keeps the alias from reaching into a dependency's internals,
 * which is a tidier boundary than growing the core shim to satisfy
 * whatever a third-party package happens to import.
 *
 * Under `demo-client-app` the toasts are REAL. The plugin turned out
 * not to use IPC for any of this — `sendNotification` is
 * `new window.Notification(...)`, which Tauri's WebView intercepts and
 * renders as a native Windows toast — so the shim can reproduce it
 * exactly rather than approximate it. That matters for a promo demo:
 * a notice arriving as an actual toast, outside the app window, is the
 * part of this product a screenshot of the app cannot show.
 */

type NotificationOptions = {
  title?: string;
  body?: string;
  [key: string]: unknown;
};

/** True inside the Tauri shell, false in the browser demo. */
const inShell = (): boolean => '__TAURI_INTERNALS__' in window;

const realInvoke = <T>(cmd: string): Promise<T> =>
  (window as unknown as { __TAURI_INTERNALS__: { invoke: (c: string) => Promise<T> } })
    .__TAURI_INTERNALS__.invoke(cmd);

/**
 * Mirrors the real plugin: consult the Web Notification permission, and
 * only fall back to the plugin command while it is still `default`.
 *
 * In the browser we answer `true` unconditionally rather than prompt —
 * a permission dialog on load is the first thing a viewer would see,
 * and the demo has nothing to gain from it when nothing will toast.
 */
export async function isPermissionGranted(): Promise<boolean> {
  if (!inShell()) return true;
  if (window.Notification.permission !== 'default') {
    return window.Notification.permission === 'granted';
  }
  return realInvoke<boolean>('plugin:notification|is_permission_granted');
}

export async function requestPermission(): Promise<NotificationPermission> {
  if (!inShell()) return 'granted';
  return window.Notification.requestPermission();
}

/**
 * Real toast in the app window; logged in the browser, where seeing it
 * fire in the console is how you tell "the demo doesn't toast" apart
 * from "the app decided not to".
 */
export function sendNotification(options: NotificationOptions | string): void {
  if (inShell()) {
    if (typeof options === 'string') new window.Notification(options);
    else new window.Notification(options.title ?? '', options);
    return;
  }
  const o = typeof options === 'string' ? { title: options } : options;
  console.info('[demo-client] OS toast:', o.title ?? '', o.body ?? '');
}

/**
 * `onAction` is desktop-unimplemented in real Tauri too — the product
 * already treats a rejection here as normal (a toast that can't carry a
 * click handler still shows). Resolving with a no-op unlisten keeps the
 * demo on the same path without inventing behaviour the product lacks.
 */
export async function onAction(_cb: (n: unknown) => void): Promise<() => void> {
  return () => {};
}
