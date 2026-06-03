import { LogIn } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation, useNavigate } from 'react-router-dom';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth, type Role } from '@/lib/auth';

type LocationState = { from?: { pathname?: string } };

type LoginResp = {
  token: string;
  role: Role;
  must_change_pw: boolean;
  exp: number;
};

export function Login() {
  const { setToken, isAuthenticated } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const { t } = useTranslation('login');

  const from = (location.state as LocationState | null)?.from?.pathname ?? '/dashboard';

  // If the user lands here while already authenticated (e.g. typed
  // /login by hand), kick them to the dashboard instead of showing
  // the form. This also catches the post-setToken render.
  useEffect(() => {
    if (isAuthenticated) {
      navigate(from, { replace: true });
    }
  }, [isAuthenticated, from, navigate]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!username.trim() || !password) return;
    setBusy(true);
    setError('');
    try {
      const resp = await apiFetch<LoginResp>('/api/auth/login', {
        method: 'POST',
        body: JSON.stringify({ username: username.trim(), password }),
      });
      setToken(resp.token);
      // A freshly-seeded / reset account is forced to pick a new
      // password before doing anything else.
      navigate(resp.must_change_pw ? '/change-password' : from, { replace: true });
    } catch (err) {
      // 401 already cleared the (empty) token + fired the expired
      // event; we still surface the message inline so the operator
      // knows the credentials were wrong rather than seeing a blank
      // re-render.
      setError(formatError(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="min-h-screen flex items-center justify-center px-4 py-8 bg-bg">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="flex justify-center mb-3">
            <picture>
              <source media="(prefers-color-scheme: dark)" srcSet="/icon-dark.svg" />
              <img src="/icon.svg" alt={t('iconAlt')} className="h-12 w-auto" />
            </picture>
          </div>
          <CardTitle className="text-2xl">
            <span className="bg-gradient-to-br from-violet via-amber to-teal bg-clip-text text-transparent">
              {t('title')}
            </span>
          </CardTitle>
          <CardDescription>{t('subtitle')}</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="space-y-4">
            <div className="space-y-1">
              <Label htmlFor="login-username">{t('usernameLabel')}</Label>
              <Input
                id="login-username"
                autoFocus
                autoComplete="username"
                placeholder={t('usernamePlaceholder')}
                value={username}
                onChange={(e) => setUsername(e.target.value)}
              />
            </div>
            <div className="space-y-1">
              <Label htmlFor="login-password">{t('passwordLabel')}</Label>
              <Input
                id="login-password"
                type="password"
                autoComplete="current-password"
                placeholder={t('passwordPlaceholder')}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>
            {error && <p className="text-sm text-red-500">{error}</p>}
            <Button type="submit" disabled={busy || !username.trim() || !password} className="w-full">
              <LogIn className="size-4 mr-2" />
              {busy ? t('submitting') : t('submit')}
            </Button>
          </form>
          <p className="mt-4 text-xs text-muted">{t('hint')}</p>
        </CardContent>
      </Card>
    </main>
  );
}
