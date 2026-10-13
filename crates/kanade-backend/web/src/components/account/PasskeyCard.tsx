import { Fingerprint, Trash2 } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiFetch, ApiError, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import { createPasskey, isCancellation, passkeysSupported } from '@/lib/passkey';
import { usePasskeyEnabled } from '@/lib/usePasskeyEnabled';

type PasskeyRow = { id: number; name: string; created_at: string; last_used_at: string | null };

type StartResp = { challenge_id: string; options: { publicKey: Record<string, unknown> } };

/** SQLite `CURRENT_TIMESTAMP` is `YYYY-MM-DD HH:MM:SS` in UTC. */
function fmt(ts: string): string {
  const d = new Date(`${ts.replace(' ', 'T')}Z`);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString();
}

/** The server's message without the `status —` prefix `formatError` adds. */
function serverMessage(err: unknown): string {
  return err instanceof ApiError && err.body ? err.body : formatError(err);
}

/**
 * Self-service passkey (WebAuthn) management: list, add, remove. Renders
 * nothing unless the backend has `[server.webauthn]` configured.
 *
 * Adding or removing a credential re-authenticates with the current password
 * (and a TOTP code when MFA is on) — the backend enforces this; the fields
 * here just collect it. That is what keeps a hijacked session from enrolling
 * the attacker's device, and from sidestepping TOTP via a passkey.
 */
export function PasskeyCard() {
  const { t } = useTranslation('passkey');
  const enabled = usePasskeyEnabled();
  const { mfaEnabled, mustChangePw } = useAuth();
  const supported = passkeysSupported();

  const [rows, setRows] = useState<PasskeyRow[]>([]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [totp, setTotp] = useState('');
  const [busy, setBusy] = useState(false);
  // Row whose removal form is open.
  const [removing, setRemoving] = useState<PasskeyRow | null>(null);

  const load = useCallback(async () => {
    try {
      setRows(await apiFetch<PasskeyRow[]>('/api/auth/passkeys'));
    } catch (err) {
      toast.error(formatError(err));
    }
  }, []);

  useEffect(() => {
    if (enabled) void load();
  }, [enabled, load]);

  if (!enabled) return null;

  function reset() {
    setAdding(false);
    setRemoving(null);
    setName('');
    setPassword('');
    setTotp('');
  }

  const reauth = () => ({
    password,
    ...(mfaEnabled && totp.trim() ? { totp_code: totp.trim() } : {}),
  });
  const reauthReady = password.length > 0 && (!mfaEnabled || totp.trim().length > 0);

  async function register(e: React.FormEvent) {
    e.preventDefault();
    if (!name.trim() || !reauthReady || busy) return;
    setBusy(true);
    try {
      const started = await apiFetch<StartResp>('/api/auth/passkeys/register/start', {
        method: 'POST',
        body: JSON.stringify({ name: name.trim(), ...reauth() }),
      });
      // The server-side proof is spent; don't keep the password in state while
      // the browser prompt is open.
      setPassword('');
      setTotp('');
      const credential = await createPasskey(started.options);
      await apiFetch('/api/auth/passkeys/register/finish', {
        method: 'POST',
        body: JSON.stringify({ challenge_id: started.challenge_id, credential }),
      });
      toast.success(t('registered'));
      reset();
      await load();
    } catch (err) {
      if (isCancellation(err)) {
        toast.message(t('registerCancelled'));
        reset();
      } else if (err instanceof ApiError) {
        toast.error(serverMessage(err));
      } else {
        toast.error(`${t('registerFailed')} ${String(err)}`);
      }
    } finally {
      setBusy(false);
    }
  }

  async function remove(e: React.FormEvent) {
    e.preventDefault();
    if (!removing || !reauthReady || busy) return;
    setBusy(true);
    try {
      await apiFetch(`/api/auth/passkeys/${removing.id}/remove`, {
        method: 'POST',
        body: JSON.stringify(reauth()),
      });
      toast.success(t('removed'));
      reset();
      await load();
    } catch (err) {
      toast.error(serverMessage(err));
    } finally {
      setBusy(false);
    }
  }

  const reauthFields = (
    <>
      <div className="space-y-1">
        <Label htmlFor="passkey-password">{t('passwordLabel')}</Label>
        <Input
          id="passkey-password"
          type="password"
          autoComplete="current-password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
      </div>
      {mfaEnabled && (
        <div className="space-y-1">
          <Label htmlFor="passkey-totp">{t('totpLabel')}</Label>
          <Input
            id="passkey-totp"
            inputMode="numeric"
            autoComplete="one-time-code"
            pattern="[0-9]*"
            maxLength={6}
            value={totp}
            onChange={(e) => setTotp(e.target.value.replace(/\D/g, ''))}
          />
          <p className="text-xs text-muted">{t('totpHint')}</p>
        </div>
      )}
    </>
  );

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Fingerprint className="size-5 text-teal" />
          {t('cardTitle')}
        </CardTitle>
        <CardDescription>{t('cardSubtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {!supported && (
          <p role="status" className="text-sm text-amber">
            {t('unsupported')}
          </p>
        )}
        {mfaEnabled && <p className="text-xs text-muted">{t('mfaNote')}</p>}

        {rows.length === 0 ? (
          <p className="text-sm text-muted">{t('empty')}</p>
        ) : (
          <ul className="divide-y divide-border rounded-md border border-border">
            {rows.map((r) => (
              <li key={r.id} className="flex items-center justify-between gap-3 px-3 py-2">
                <div className="min-w-0">
                  <div className="truncate text-sm font-medium">{r.name}</div>
                  <div className="text-xs text-muted">
                    {t('createdAt', { when: fmt(r.created_at) })} ·{' '}
                    {r.last_used_at
                      ? t('lastUsed', { when: fmt(r.last_used_at) })
                      : t('neverUsed')}
                  </div>
                </div>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  aria-label={`${t('remove')}: ${r.name}`}
                  disabled={busy}
                  onClick={() => {
                    reset();
                    setRemoving(r);
                  }}
                >
                  <Trash2 className="size-4" />
                </Button>
              </li>
            ))}
          </ul>
        )}

        {removing && (
          <form onSubmit={remove} className="max-w-md space-y-3 rounded-md border border-border p-3">
            <p className="text-sm font-medium">{t('removeTitle', { name: removing.name })}</p>
            <p className="text-xs text-muted">{t('removeHint')}</p>
            {reauthFields}
            <div className="flex gap-2">
              <Button type="submit" variant="danger" disabled={busy || !reauthReady}>
                {busy ? t('removing') : t('removeConfirm')}
              </Button>
              <Button type="button" variant="secondary" onClick={reset} disabled={busy}>
                {t('cancel')}
              </Button>
            </div>
          </form>
        )}

        {adding ? (
          <form onSubmit={register} className="max-w-md space-y-3">
            <div className="space-y-1">
              <Label htmlFor="passkey-name">{t('nameLabel')}</Label>
              <Input
                id="passkey-name"
                autoFocus
                maxLength={64}
                placeholder={t('namePlaceholder')}
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            </div>
            {reauthFields}
            <p className="text-xs text-muted">
              {t('reauthHint', { totp: mfaEnabled ? t('reauthTotp') : '' })}
            </p>
            <div className="flex gap-2">
              <Button
                type="submit"
                disabled={busy || !supported || !name.trim() || !reauthReady}
                className="flex-1"
              >
                {busy ? t('registering') : t('register')}
              </Button>
              <Button type="button" variant="secondary" onClick={reset} disabled={busy}>
                {t('cancel')}
              </Button>
            </div>
          </form>
        ) : (
          !removing && (
            <div className="space-y-1">
              <Button
                type="button"
                onClick={() => setAdding(true)}
                disabled={!supported || mustChangePw || busy}
                className="w-full max-w-md"
              >
                <Fingerprint className="size-4 mr-2" />
                {t('add')}
              </Button>
              {mustChangePw && <p className="text-xs text-muted">{t('mustChangePw')}</p>}
            </div>
          )
        )}
      </CardContent>
    </Card>
  );
}
