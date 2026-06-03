import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import { useLocation, useNavigate } from 'react-router-dom';

import { apiFetch, AUTH_EXPIRED_EVENT } from '@/lib/api';

const TOKEN_KEY = 'kanade_token';

/** RBAC role tiers. `admin ⊇ operator ⊇ viewer`. */
export type Role = 'viewer' | 'operator' | 'admin';
const RANK: Record<Role, number> = { viewer: 0, operator: 1, admin: 2 };

type Me = { username: string; role: Role };

type AuthContextValue = {
  token: string;
  username: string | null;
  role: Role | null;
  setToken: (next: string) => void;
  logout: () => void;
  isAuthenticated: boolean;
  /** True when the signed-in caller's role is at least `min`. */
  hasRole: (min: Role) => boolean;
};

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [token, setTokenState] = useState<string>(() => localStorage.getItem(TOKEN_KEY) ?? '');
  // Identity + role, resolved from `GET /api/auth/me` whenever the token
  // changes. Drives UI gating (hide operator/admin actions); the backend
  // is the real enforcement boundary.
  const [me, setMe] = useState<Me | null>(null);
  const navigate = useNavigate();
  const location = useLocation();

  const setToken = useCallback((next: string) => {
    if (next) {
      localStorage.setItem(TOKEN_KEY, next);
    } else {
      localStorage.removeItem(TOKEN_KEY);
    }
    setTokenState(next);
  }, []);

  const logout = useCallback(() => {
    setToken('');
    navigate('/login', { replace: true });
  }, [setToken, navigate]);

  // Resolve identity whenever the token changes. A failed lookup (e.g.
  // the 401 path below) just clears the role — the token-clear + redirect
  // is handled by the AUTH_EXPIRED_EVENT listener.
  useEffect(() => {
    let cancelled = false;
    if (!token) {
      setMe(null);
      return;
    }
    apiFetch<Me>('/api/auth/me')
      .then((m) => {
        if (!cancelled) setMe(m);
      })
      .catch(() => {
        if (!cancelled) setMe(null);
      });
    return () => {
      cancelled = true;
    };
  }, [token]);

  // Backend 401 path: apiFetch fires a window event when a query
  // gets a 401. Catch it here so we can drop the in-memory token
  // and route to /login while preserving the location the user
  // was on (so post-login they land back where they were).
  useEffect(() => {
    const handler = () => {
      setTokenState('');
      setMe(null);
      // Skip the redirect if we're already on /login — no useful
      // location to preserve, and double-pushing the same route
      // would noise up the history stack.
      if (location.pathname !== '/login') {
        navigate('/login', { state: { from: location }, replace: true });
      }
    };
    window.addEventListener(AUTH_EXPIRED_EVENT, handler);
    return () => window.removeEventListener(AUTH_EXPIRED_EVENT, handler);
  }, [navigate, location]);

  const value = useMemo<AuthContextValue>(
    () => ({
      token,
      username: me?.username ?? null,
      role: me?.role ?? null,
      setToken,
      logout,
      isAuthenticated: token.length > 0,
      hasRole: (min: Role) => (me ? RANK[me.role] >= RANK[min] : false),
    }),
    [token, me, setToken, logout],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth() {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used inside <AuthProvider>');
  return ctx;
}
