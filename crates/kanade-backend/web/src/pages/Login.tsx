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

// The backend's login reply is an untagged union: either a full session
// (has `token`) or a prompt for a second factor (`mfa_required: true`,
// no token) when the password was correct but the account has TOTP on.
type MfaRequired = { mfa_required: true };
type LoginOutcome = LoginResp | MfaRequired;

function needsMfa(o: LoginOutcome): o is MfaRequired {
  return 'mfa_required' in o && !('token' in o);
}

export function Login() {
  const { setToken, isAuthenticated } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [totpCode, setTotpCode] = useState('');
  // Set once the server answers `mfa_required` for a correct password:
  // flips the form to a second step asking for the authenticator code.
  const [mfaStep, setMfaStep] = useState(false);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  // Self-service password reset: toggles the form to a username-only
  // "send me a reset link" view. The response is deliberately uniform
  // (no account-existence disclosure), so success just shows a fixed note.
  const [mode, setMode] = useState<'login' | 'forgot'>('login');
  const [forgotSent, setForgotSent] = useState(false);
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
    // On the second step the code is required; don't fire a request that
    // would just come back mfa_required again.
    if (mfaStep && !totpCode.trim()) return;
    setBusy(true);
    setError('');
    try {
      const resp = await apiFetch<LoginOutcome>('/api/auth/login', {
        method: 'POST',
        body: JSON.stringify({
          username: username.trim(),
          password,
          // Only send a code on the second step; omit it on the first so
          // a code-less first attempt gets the mfa_required prompt.
          ...(mfaStep && totpCode.trim() ? { totp_code: totpCode.trim() } : {}),
        }),
      });
      if (needsMfa(resp)) {
        // Correct password, MFA on: reveal the code field and stop.
        setMfaStep(true);
        setError('');
        return;
      }
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

  async function submitForgot(e: React.FormEvent) {
    e.preventDefault();
    if (!username.trim()) return;
    setBusy(true);
    try {
      // Always 200 server-side; we don't even branch on the result so
      // timing/behaviour can't reveal whether the account exists.
      await apiFetch('/api/auth/forgot-password', {
        method: 'POST',
        body: JSON.stringify({ username: username.trim() }),
      });
    } catch {
      // Swallow — surfacing an error would leak existence/state. The
      // uniform "if the account exists…" note covers every outcome.
    } finally {
      setForgotSent(true);
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
          <CardDescription>{mode === 'forgot' ? t('forgotTitle') : t('subtitle')}</CardDescription>
        </CardHeader>
        <CardContent>
          {mode === 'login' ? (
            <>
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
                    disabled={mfaStep}
                  />
                </div>
                {mfaStep && (
                  <div className="space-y-1">
                    <Label htmlFor="login-totp">{t('mfaLabel')}</Label>
                    <Input
                      id="login-totp"
                      autoFocus
                      inputMode="numeric"
                      autoComplete="one-time-code"
                      pattern="[0-9]*"
                      maxLength={6}
                      placeholder={t('mfaPlaceholder')}
                      value={totpCode}
                      onChange={(e) => setTotpCode(e.target.value.replace(/\D/g, ''))}
                    />
                    <p className="text-xs text-muted">{t('mfaHint')}</p>
                  </div>
                )}
                {error && <p className="text-sm text-red-500">{error}</p>}
                <Button
                  type="submit"
                  disabled={
                    busy || !username.trim() || !password || (mfaStep && !totpCode.trim())
                  }
                  className="w-full"
                >
                  <LogIn className="size-4 mr-2" />
                  {busy ? t('submitting') : mfaStep ? t('mfaSubmit') : t('submit')}
                </Button>
              </form>
              <button
                type="button"
                className="mt-3 text-xs text-muted hover:text-fg underline"
                onClick={() => {
                  setMode('forgot');
                  setError('');
                }}
              >
                {t('forgot')}
              </button>
              <p className="mt-4 text-xs text-muted">{t('hint')}</p>
            </>
          ) : forgotSent ? (
            <div className="space-y-4">
              <p className="text-sm">{t('forgotSent')}</p>
              <Button
                variant="secondary"
                className="w-full"
                onClick={() => {
                  setMode('login');
                  setForgotSent(false);
                }}
              >
                {t('backToLogin')}
              </Button>
            </div>
          ) : (
            <form onSubmit={submitForgot} className="space-y-4">
              <p className="text-xs text-muted">{t('forgotHint')}</p>
              <div className="space-y-1">
                <Label htmlFor="forgot-username">{t('usernameLabel')}</Label>
                <Input
                  id="forgot-username"
                  autoFocus
                  autoComplete="username"
                  placeholder={t('usernamePlaceholder')}
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                />
              </div>
              <Button type="submit" disabled={busy || !username.trim()} className="w-full">
                {busy ? t('forgotSending') : t('forgotSubmit')}
              </Button>
              <button
                type="button"
                className="text-xs text-muted hover:text-fg underline"
                onClick={() => setMode('login')}
              >
                {t('backToLogin')}
              </button>
            </form>
          )}
        </CardContent>
      </Card>
    </main>
  );
}
