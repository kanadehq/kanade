import { useEffect, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiFetch, formatError } from '@/lib/api';

type Passkeys = { enabled: boolean; rp_id: string; rp_origins: string[]; rp_name: string | null };
const empty: Passkeys = { enabled: false, rp_id: '', rp_origins: [], rp_name: null };

export function PasskeySettingsCard({ canOperate }: { canOperate: boolean }) {
  const { t } = useTranslation('settings');
  const client = useQueryClient();
  const settings = useQuery({
    queryKey: ['server-settings'],
    queryFn: () => apiFetch<{ passkeys?: Passkeys }>('/api/server-settings'),
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  const status = useQuery({
    queryKey: ['passkey-status'],
    queryFn: () => apiFetch<{ state: string; reason?: string }>('/api/server-settings/passkeys/status'),
    retry: false,
  });
  const [draft, setDraft] = useState(empty);
  const [origins, setOrigins] = useState('');
  useEffect(() => {
    if (!settings.data) return;
    const value = { ...empty, ...settings.data.passkeys };
    setDraft(value);
    setOrigins(value.rp_origins.join('\n'));
  }, [settings.data]);
  const value = { ...draft, rp_origins: origins.split('\n').map((v) => v.trim()).filter(Boolean) };
  const stored = { ...empty, ...settings.data?.passkeys };
  // Send only edited inner fields, preserving simultaneous edits to other fields.
  const patch = Object.fromEntries((Object.keys(value) as (keyof Passkeys)[])
    .filter((key) => JSON.stringify(value[key]) !== JSON.stringify(stored[key]))
    .map((key) => [key, value[key]]));
  const save = useMutation({
    mutationFn: () => apiFetch('/api/server-settings', { method: 'PUT', body: JSON.stringify({ passkeys: patch }) }),
    onSuccess: async () => {
      toast.success(t('server.saved'));
      await Promise.all([
        client.invalidateQueries({ queryKey: ['server-settings'], exact: true }),
        client.invalidateQueries({ queryKey: ['passkey-config'] }),
        client.invalidateQueries({ queryKey: ['passkey-status'] }),
      ]);
    },
    onError: (error) => toast.error(formatError(error)),
  });
  const disabled = !canOperate || !settings.data || save.isPending;
  return <Card>
    <CardHeader><CardTitle>{t('server.passkeys.title')}</CardTitle></CardHeader>
    <CardContent className="space-y-3">
      <p>{t('server.passkeys.description')}</p>
      <p role="status">{t(`server.passkeys.${status.isError ? 'unavailable' : status.data?.state ?? 'loading'}`)}</p>
      {status.data?.reason && <p className="text-red-500">{status.data.reason}</p>}
      {settings.isError && <p role="alert">{formatError(settings.error)}</p>}
      <Label htmlFor="passkeys-enabled"><input id="passkeys-enabled" type="checkbox" checked={draft.enabled}
        disabled={disabled} onChange={(e) => setDraft({ ...draft, enabled: e.target.checked })} /> {t('server.passkeys.enabledLabel')}</Label>
      <Label htmlFor="passkeys-rp-id">{t('server.passkeys.rpId')}</Label>
      <Input id="passkeys-rp-id" disabled={disabled} value={draft.rp_id} onChange={(e) => setDraft({ ...draft, rp_id: e.target.value })} />
      <Label htmlFor="passkeys-origins">{t('server.passkeys.origins')}</Label>
      <textarea id="passkeys-origins" className="w-full border rounded p-2" rows={3} disabled={disabled}
        value={origins} onChange={(e) => setOrigins(e.target.value)} />
      <Label htmlFor="passkeys-rp-name">{t('server.passkeys.rpName')}</Label>
      <Input id="passkeys-rp-name" disabled={disabled} value={draft.rp_name ?? ''} placeholder="Kanade"
        onChange={(e) => setDraft({ ...draft, rp_name: e.target.value || null })} />
      {save.isError && <p role="alert">{formatError(save.error)}</p>}
      <Button disabled={disabled || Object.keys(patch).length === 0} onClick={() => save.mutate()}>{t('server.passkeys.save')}</Button>
    </CardContent>
  </Card>;
}
