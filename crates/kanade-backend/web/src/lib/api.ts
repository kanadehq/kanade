/**
 * Thin fetch wrapper that:
 *   1. Injects `Authorization: Bearer <token>` from localStorage on
 *      every /api/* call.
 *   2. Throws a typed ApiError with status + body so React Query's
 *      isError / error.message renders something useful.
 *   3. On 401, fires a `kanade:auth-expired` window event so the
 *      AuthProvider (which lives inside the React Router) can
 *      clear the in-memory token + navigate to /login. The
 *      localStorage value is cleared too so a hard refresh starts
 *      unauthenticated.
 */

export const AUTH_EXPIRED_EVENT = 'kanade:auth-expired';
const TOKEN_KEY = 'kanade_token';

export class ApiError extends Error {
  status: number;
  body: string;
  constructor(status: number, statusText: string, body: string) {
    super(`${status} ${statusText} — ${body || '(no body)'}`);
    this.status = status;
    this.body = body;
  }
}

function authHeaders(): HeadersInit {
  const token = localStorage.getItem(TOKEN_KEY) ?? '';
  return token ? { Authorization: `Bearer ${token}` } : {};
}

async function apiFetchRaw(path: string, init: RequestInit = {}): Promise<{ res: Response; text: string }> {
  const headers = new Headers(init.headers ?? {});
  for (const [k, v] of Object.entries(authHeaders())) headers.set(k, v as string);
  // Lets the backend tag operator-initiated audit events with
  // `source: "spa"` so the audit log distinguishes browser-driven
  // actions from CLI ones without rewriting handler signatures.
  if (!headers.has('X-Kanade-Source')) headers.set('X-Kanade-Source', 'spa');
  if (init.body && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json');
  }

  const res = await fetch(path, { ...init, headers });
  const text = await res.text();

  if (res.status === 401) {
    localStorage.removeItem(TOKEN_KEY);
    // React-side listener (AuthProvider) picks this up and routes
    // to /login. We still throw the ApiError so the calling query
    // surfaces an error state instead of returning undefined.
    window.dispatchEvent(new Event(AUTH_EXPIRED_EVENT));
  }
  if (!res.ok) {
    throw new ApiError(res.status, res.statusText, text);
  }
  return { res, text };
}

export async function apiFetch<T = unknown>(path: string, init: RequestInit = {}): Promise<T> {
  const { text } = await apiFetchRaw(path, init);
  return text ? (JSON.parse(text) as T) : (undefined as T);
}

/**
 * Variant of `apiFetch` for endpoints that return non-JSON bodies
 * (e.g. `GET /api/{jobs,schedules}/{id}/yaml`, which streams raw
 * YAML). Reuses the same auth / source-tag plumbing — handlers
 * shouldn't be re-implementing the bearer-token + X-Kanade-Source
 * dance per consumer.
 */
export async function apiFetchText(path: string, init: RequestInit = {}): Promise<string> {
  const { text } = await apiFetchRaw(path, init);
  return text;
}

// Pretty-prints whatever React Query handed back as the error. ApiError
// already carries the status + body; anything else (network error, JSON
// parse failure, etc.) falls back to its String() form.
export function formatError(err: unknown): string {
  return err instanceof ApiError ? `${err.status} — ${err.body || err.message}` : String(err);
}
