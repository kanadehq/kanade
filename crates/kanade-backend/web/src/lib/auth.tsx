import { createContext, useCallback, useContext, useMemo, useState, type ReactNode } from 'react';

const TOKEN_KEY = 'kanade_token';

type AuthContextValue = {
  token: string;
  setToken: (next: string) => void;
  isAuthenticated: boolean;
};

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [token, setTokenState] = useState<string>(() => localStorage.getItem(TOKEN_KEY) ?? '');

  const setToken = useCallback((next: string) => {
    if (next) {
      localStorage.setItem(TOKEN_KEY, next);
    } else {
      localStorage.removeItem(TOKEN_KEY);
    }
    setTokenState(next);
  }, []);

  const value = useMemo<AuthContextValue>(
    () => ({ token, setToken, isAuthenticated: token.length > 0 }),
    [token, setToken],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth() {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used inside <AuthProvider>');
  return ctx;
}
