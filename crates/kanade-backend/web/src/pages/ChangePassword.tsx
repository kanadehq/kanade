import { KeyRound } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';

const MIN_PASSWORD_LEN = 8;

export function ChangePassword() {
  const navigate = useNavigate();
  const { refresh } = useAuth();
  const { t } = useTranslation('accounts');
  const [oldPw, setOldPw] = useState('');
  const [newPw, setNewPw] = useState('');
  const [confirmPw, setConfirmPw] = useState('');
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (newPw !== confirmPw) {
      toast.error(t('changePw.mismatch'));
      return;
    }
    if (newPw.length < MIN_PASSWORD_LEN) {
      toast.error(t('changePw.tooShort'));
      return;
    }
    setBusy(true);
    try {
      await apiFetch('/api/auth/change-password', {
        method: 'POST',
        body: JSON.stringify({ old_password: oldPw, new_password: newPw }),
      });
      toast.success(t('changePw.success'));
      // Re-fetch identity so the must_change_pw gate in ProtectedLayout
      // releases — otherwise navigating to /dashboard bounces straight
      // back here off the stale flag.
      await refresh();
      navigate('/dashboard', { replace: true });
    } catch (err) {
      toast.error(formatError(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="min-h-screen flex items-center justify-center px-4 py-8 bg-bg">
      <Card className="w-full max-w-md">
        <CardHeader>
          <CardTitle>{t('changePw.title')}</CardTitle>
          <CardDescription>{t('changePw.subtitle')}</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="space-y-4">
            <div className="space-y-1">
              <Label htmlFor="old-pw">{t('changePw.old')}</Label>
              <Input
                id="old-pw"
                type="password"
                autoComplete="current-password"
                value={oldPw}
                onChange={(e) => setOldPw(e.target.value)}
              />
            </div>
            <div className="space-y-1">
              <Label htmlFor="new-pw">{t('changePw.new')}</Label>
              <Input
                id="new-pw"
                type="password"
                autoComplete="new-password"
                value={newPw}
                onChange={(e) => setNewPw(e.target.value)}
              />
              <p className="text-xs text-muted">{t('passwordHint')}</p>
            </div>
            <div className="space-y-1">
              <Label htmlFor="confirm-pw">{t('changePw.confirm')}</Label>
              <Input
                id="confirm-pw"
                type="password"
                autoComplete="new-password"
                value={confirmPw}
                onChange={(e) => setConfirmPw(e.target.value)}
              />
            </div>
            <Button
              type="submit"
              disabled={busy || !oldPw || newPw.length < MIN_PASSWORD_LEN || newPw !== confirmPw}
              className="w-full"
            >
              <KeyRound className="size-4 mr-2" />
              {t('changePw.submit')}
            </Button>
          </form>
        </CardContent>
      </Card>
    </main>
  );
}
