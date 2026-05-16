/**
 * Thin fetch wrapper that:
 *   1. Injects `Authorization: Bearer <token>` from localStorage on
 *      every /api/* call.
 *   2. Throws a typed ApiError with status + body so React Query's
 *      isError / error.message renders something useful.
 *   3. On 401, drops the stored token so the next render shows the
 *      login dialog. The component-side useAuth() is the
 *      authoritative source — this just removes the localStorage
 *      copy; the React state catches up on the next focus.
 */

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
  if (init.body && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json');
  }

  const res = await fetch(path, { ...init, headers });
  const text = await res.text();

  if (res.status === 401) {
    localStorage.removeItem(TOKEN_KEY);
  }
  if (!res.ok) {
    throw new ApiError(res.status, res.statusText, text);
  }
  return text ? (JSON.parse(text) as T) : (undefined as T);
}
