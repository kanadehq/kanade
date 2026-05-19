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

export async function apiFetch<T = unknown>(path: string, init: RequestInit = {}): Promise<T> {
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
  return text ? (JSON.parse(text) as T) : (undefined as T);
}
