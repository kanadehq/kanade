import { ShieldCheck, ShieldOff } from 'lucide-react';
import { QRCodeSVG } from 'qrcode.react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';

type InitResp = { secret: string; otpauth_url: string };

/**
 * Self-service TOTP MFA enrollment card (#1192). Backend contract:
 *   POST /api/auth/mfa/init    → { secret, otpauth_url }  (nothing persisted)
 *   POST /api/auth/mfa/verify  { secret, code, current_code? } → 204  (activates)
 *   POST /api/auth/mfa/disable { code } → 204
 * A candidate secret is only stored once a live code confirms it, so an
 * abandoned enrolment leaves the account untouched. Re-enrolling while MFA
 * is already on is a rotation and additionally requires a current code —
 * the backend enforces this, we surface the field to match.
 *
 * Rendered as a plain Card so it can sit inside the in-app Account page
 * (no full-viewport centering — that's for the standalone auth screens).
 */
export function MfaCard() {
  const { t } = useTranslation('security');
  const { mfaEnabled, refresh } = useAuth();

  // The candidate secret from mfa/init while enrolling; null when idle.
  const [enroll, setEnroll] = useState<InitResp | null>(null);
  const [code, setCode] = useState('');
  // Rotation / disable both need a code from the *currently active* secret.
  const [currentCode, setCurrentCode] = useState('');
  const [busy, setBusy] = useState(false);

  async function begin() {
    setBusy(true);
    setCode('');
    setCurrentCode('');
    try {
      setEnroll(await apiFetch<InitResp>('/api/auth/mfa/init', { method: 'POST' }));
    } catch (err) {
      toast.error(formatError(err));
    } finally {
      setBusy(false);
    }
  }

  function cancel() {
    setEnroll(null);
    setCode('');
    setCurrentCode('');
  }

  async function copySecret(secret: string) {
    // Only claim success once the write actually resolves — the setup key
    // is the operator's one chance to seed their authenticator, so a false
    // "Copied" (insecure origin → no clipboard API, or a rejected write)
    // could leave them without it.
    try {
      await navigator.clipboard.writeText(secret);
      toast.success(t('copied'));
    } catch {
      toast.error(t('copyFailed'));
    }
  }

  async function confirm(e: React.FormEvent) {
    e.preventDefault();
    if (!enroll || !code.trim()) return;
    // Rotation (MFA already on) needs a current code too.
    if (mfaEnabled && !currentCode.trim()) return;
    setBusy(true);
    try {
      await apiFetch('/api/auth/mfa/verify', {
        method: 'POST',
        body: JSON.stringify({
          secret: enroll.secret,
          code: code.trim(),
          ...(mfaEnabled && currentCode.trim() ? { current_code: currentCode.trim() } : {}),
        }),
      });
      toast.success(t('enabledToast'));
      cancel();
      await refresh();
    } catch (err) {
      toast.error(formatError(err));
    } finally {
      setBusy(false);
    }
  }

  async function disable(e: React.FormEvent) {
    e.preventDefault();
    if (!currentCode.trim()) return;
    setBusy(true);
    try {
      await apiFetch('/api/auth/mfa/disable', {
        method: 'POST',
        body: JSON.stringify({ code: currentCode.trim() }),
      });
      toast.success(t('disabledToast'));
      setCurrentCode('');
      await refresh();
    } catch (err) {
      toast.error(formatError(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          {mfaEnabled ? (
            <ShieldCheck className="size-5 text-teal" />
          ) : (
            <ShieldOff className="size-5 text-muted" />
          )}
          {t('title')}
        </CardTitle>
        <CardDescription>{t('subtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        <p className="text-sm">{mfaEnabled ? t('statusOn') : t('statusOff')}</p>

        {enroll ? (
          // ---- Enrollment (also used for rotation) ---------------------
          <form onSubmit={confirm} className="max-w-md space-y-4">
            <div className="space-y-2">
              <Label>{t('scanTitle')}</Label>
              <div className="flex justify-center rounded-md bg-white p-4">
                {/* Rendered in a white box so the QR stays scannable in dark mode. */}
                <QRCodeSVG value={enroll.otpauth_url} size={176} marginSize={0} />
              </div>
              <p className="text-xs text-muted">{t('scanHint')}</p>
            </div>

            <div className="space-y-1">
              <Label htmlFor="mfa-secret">{t('secretLabel')}</Label>
              <div className="flex items-center gap-2">
                <Input
                  id="mfa-secret"
                  readOnly
                  value={enroll.secret}
                  className="font-mono text-xs"
                  onFocus={(e) => e.currentTarget.select()}
                />
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  onClick={() => void copySecret(enroll.secret)}
                >
                  {t('copyKey')}
                </Button>
              </div>
            </div>

            {mfaEnabled && (
              <div className="space-y-1">
                <Label htmlFor="mfa-current">{t('currentCodeLabel')}</Label>
                <Input
                  id="mfa-current"
                  inputMode="numeric"
                  autoComplete="one-time-code"
                  pattern="[0-9]*"
                  maxLength={6}
                  placeholder={t('codePlaceholder')}
                  value={currentCode}
                  onChange={(e) => setCurrentCode(e.target.value.replace(/\D/g, ''))}
                />
                <p className="text-xs text-muted">{t('currentCodeHint')}</p>
              </div>
            )}

            <div className="space-y-1">
              <Label htmlFor="mfa-code">{t('codeLabel')}</Label>
              <Input
                id="mfa-code"
                autoFocus
                inputMode="numeric"
                autoComplete="one-time-code"
                pattern="[0-9]*"
                maxLength={6}
                placeholder={t('codePlaceholder')}
                value={code}
                onChange={(e) => setCode(e.target.value.replace(/\D/g, ''))}
              />
            </div>

            <div className="flex gap-2">
              <Button
                type="submit"
                disabled={busy || !code.trim() || (mfaEnabled && !currentCode.trim())}
                className="flex-1"
              >
                {busy ? t('confirming') : t('confirm')}
              </Button>
              <Button type="button" variant="secondary" onClick={cancel} disabled={busy}>
                {t('cancel')}
              </Button>
            </div>
          </form>
        ) : mfaEnabled ? (
          // ---- Enabled: rotate or disable ------------------------------
          <div className="max-w-md space-y-6">
            <Button variant="secondary" onClick={begin} disabled={busy} className="w-full">
              {t('reenroll')}
            </Button>
            <form onSubmit={disable} className="space-y-3 border-t border-border pt-4">
              <div className="space-y-1">
                <Label htmlFor="mfa-disable-code">{t('disableTitle')}</Label>
                <Input
                  id="mfa-disable-code"
                  inputMode="numeric"
                  autoComplete="one-time-code"
                  pattern="[0-9]*"
                  maxLength={6}
                  placeholder={t('codePlaceholder')}
                  value={currentCode}
                  onChange={(e) => setCurrentCode(e.target.value.replace(/\D/g, ''))}
                />
                <p className="text-xs text-muted">{t('disableHint')}</p>
              </div>
              <Button
                type="submit"
                variant="danger"
                disabled={busy || !currentCode.trim()}
                className="w-full"
              >
                {busy ? t('disabling') : t('disable')}
              </Button>
            </form>
          </div>
        ) : (
          // ---- Disabled: start enrollment ------------------------------
          <Button onClick={begin} disabled={busy} className="w-full max-w-md">
            <ShieldCheck className="size-4 mr-2" />
            {t('enable')}
          </Button>
        )}
      </CardContent>
    </Card>
  );
}
