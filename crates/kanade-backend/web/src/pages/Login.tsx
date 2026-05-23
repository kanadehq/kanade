import { LogIn } from 'lucide-react';
import { useEffect, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { useLocation, useNavigate } from 'react-router-dom';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { useAuth } from '@/lib/auth';

type LocationState = { from?: { pathname?: string } };

export function Login() {
  const { setToken, isAuthenticated } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const [draft, setDraft] = useState('');
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
          <form
            onSubmit={(e) => {
              e.preventDefault();
              const tok = draft.trim();
              if (!tok) return;
              setToken(tok);
              navigate(from, { replace: true });
            }}
            className="space-y-4"
          >
            <div className="space-y-1">
              <Label htmlFor="login-token">{t('tokenLabel')}</Label>
              <Input
                id="login-token"
                type="password"
                autoFocus
                autoComplete="off"
                placeholder={t('tokenPlaceholder')}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
              />
            </div>
            <Button type="submit" disabled={!draft.trim()} className="w-full">
              <LogIn className="size-4 mr-2" />
              {t('submit')}
            </Button>
          </form>
          <p className="mt-4 text-xs text-muted">
            <Trans ns="login" i18nKey="hint" components={{ code: <code /> }} />
          </p>
        </CardContent>
      </Card>
    </main>
  );
}
