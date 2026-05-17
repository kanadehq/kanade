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

import { AUTH_EXPIRED_EVENT } from '@/lib/api';

const TOKEN_KEY = 'kanade_token';

type AuthContextValue = {
  token: string;
  setToken: (next: string) => void;
  logout: () => void;
  isAuthenticated: boolean;
};

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [token, setTokenState] = useState<string>(() => localStorage.getItem(TOKEN_KEY) ?? '');
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

  // Backend 401 path: apiFetch fires a window event when a query
  // gets a 401. Catch it here so we can drop the in-memory token
  // and route to /login while preserving the location the user
  // was on (so post-login they land back where they were).
  useEffect(() => {
    const handler = () => {
      setTokenState('');
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
    () => ({ token, setToken, logout, isAuthenticated: token.length > 0 }),
    [token, setToken, logout],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth() {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used inside <AuthProvider>');
  return ctx;
}
