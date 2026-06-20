import { KeyRound } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useParams } from 'react-router-dom';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { ApiError, apiFetch, formatError } from '@/lib/api';

type TokenInfo = { username: string; purpose: 'setup' | 'reset' };

// Token validation outcome: pending → then either the resolved account
// (form shown) or why the link can't be used.
type State =
  | { kind: 'loading' }
  | { kind: 'valid'; info: TokenInfo }
  | { kind: 'invalid' }
  | { kind: 'expired' };

const MIN_PASSWORD_LEN = 8;

/**
 * Public page (outside the auth gate) for the one-time password
 * setup/reset link emailed to a user. Validates the token, then lets the
 * user set a password and bounces to /login.
 */
export function PasswordSetup() {
  const { t } = useTranslation('passwordSetup');
  const { token } = useParams<{ token: string }>();
  const navigate = useNavigate();

  const [state, setState] = useState<State>({ kind: 'loading' });
  const [pw, setPw] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  // Validate the token on mount so we render the form only for a live
  // link (and a clear message otherwise).
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const info = await apiFetch<TokenInfo>(
          `/api/auth/password-setup/${encodeURIComponent(token ?? '')}`,
        );
        if (!cancelled) setState({ kind: 'valid', info });
      } catch (err) {
        if (cancelled) return;
        setState({ kind: err instanceof ApiError && err.status === 410 ? 'expired' : 'invalid' });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [token]);

  // Count Unicode code points (like the backend's `chars().count()`), not
  // UTF-16 code units, so a password of astral chars (emoji) agrees with
  // the server's length check instead of being accepted then 400'd.
  const pwLen = [...pw].length;

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (pwLen < MIN_PASSWORD_LEN || pw !== confirm) return;
    setBusy(true);
    setError('');
    try {
      await apiFetch(`/api/auth/password-setup/${encodeURIComponent(token ?? '')}`, {
        method: 'POST',
        body: JSON.stringify({ password: pw }),
      });
      toast.success(t('toast.done'));
      navigate('/login', { replace: true });
    } catch (err) {
      // The token may have expired between validation and submit.
      if (err instanceof ApiError && (err.status === 410 || err.status === 404)) {
        setState({ kind: err.status === 410 ? 'expired' : 'invalid' });
      } else {
        setError(formatError(err));
      }
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
              <img src="/icon.svg" alt="kanade" className="h-12 w-auto" />
            </picture>
          </div>
          <CardTitle className="text-xl">{t('title')}</CardTitle>
          {state.kind === 'valid' && (
            <CardDescription>{t('subtitle', { username: state.info.username })}</CardDescription>
          )}
        </CardHeader>
        <CardContent>
          {state.kind === 'loading' && <p className="text-sm text-muted">{t('checking')}</p>}

          {(state.kind === 'invalid' || state.kind === 'expired') && (
            <div className="space-y-4">
              <p className="text-sm text-red-500">
                {state.kind === 'expired' ? t('expired') : t('invalid')}
              </p>
              <Button variant="secondary" className="w-full" onClick={() => navigate('/login')}>
                {t('toLogin')}
              </Button>
            </div>
          )}

          {state.kind === 'valid' && (
            <form onSubmit={submit} className="space-y-4">
              <div className="space-y-1">
                <Label htmlFor="ps-pw">{t('newPassword')}</Label>
                <Input
                  id="ps-pw"
                  type="password"
                  autoFocus
                  autoComplete="new-password"
                  value={pw}
                  onChange={(e) => setPw(e.target.value)}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor="ps-confirm">{t('confirmPassword')}</Label>
                <Input
                  id="ps-confirm"
                  type="password"
                  autoComplete="new-password"
                  value={confirm}
                  onChange={(e) => setConfirm(e.target.value)}
                />
              </div>
              {pwLen > 0 && pwLen < MIN_PASSWORD_LEN && (
                <p className="text-xs text-amber-500">{t('tooShort')}</p>
              )}
              {confirm.length > 0 && pw !== confirm && (
                <p className="text-xs text-amber-500">{t('mismatch')}</p>
              )}
              {error && <p className="text-sm text-red-500">{error}</p>}
              <Button
                type="submit"
                disabled={busy || pwLen < MIN_PASSWORD_LEN || pw !== confirm}
                className="w-full"
              >
                <KeyRound className="size-4 mr-2" />
                {busy ? t('submitting') : t('submit')}
              </Button>
            </form>
          )}
        </CardContent>
      </Card>
    </main>
  );
}
