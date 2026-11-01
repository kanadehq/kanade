import { useQuery } from '@tanstack/react-query';
import { ChevronDown, Copy } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { apiFetch } from '@/lib/api';

import { type InstallAttempt, type InstallEvent, newestAttempts, sortEvents, reconcileSelection, formatTimestamp } from './installHistoryModel';

export function InstallHistory() {
  const { t, i18n } = useTranslation('agent-install');
  // Undefined means the first successful response has not been selected yet.
  // Null means the selection disappeared; do not jump to another record.
  const [selected, setSelected] = useState<string | null>();
  const [removed, setRemoved] = useState(false);
  const attempts = useQuery({
    queryKey: ['install-attempts'],
    queryFn: () => apiFetch<InstallAttempt[]>('/api/agents/install-attempts'),
    refetchInterval: 5000,
  });
  const rows = newestAttempts(attempts.data ?? []);
  const attemptId = selected === undefined ? rows[0]?.id : rows.find((a) => a.id === selected)?.id;
  useEffect(() => {
    if (!attempts.isSuccess) return;
    const next = reconcileSelection(selected, attempts.data);
    if (selected && next === null) setRemoved(true);
    setSelected(next);
  }, [attempts.data, attempts.isSuccess, selected]);
  const events = useQuery({
    queryKey: ['install-events', attemptId],
    queryFn: () => apiFetch<InstallEvent[]>(`/api/agents/install-attempts/${attemptId}`),
    enabled: !!attemptId,
    refetchInterval: 5000,
  });
  const label = (group: string, value: string) => t(`logs.${group}.${value}`, { defaultValue: value });
  const date = (seconds: number) => formatTimestamp(seconds, i18n.resolvedLanguage ?? i18n.language);
  async function copyId(id: string) {
    try {
      await navigator.clipboard.writeText(id);
      toast.success(t('logs.copied'));
    } catch {
      toast.error(t('logs.copyFailed'));
    }
  }
  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('logs.title')}</CardTitle>
        <CardDescription>{t('logs.description')}</CardDescription>
      </CardHeader>
      <CardContent className="min-w-0 space-y-3">
        <p role="status" className="text-sm text-muted">
          {attempts.isPending ? t('logs.loading') : removed ? t('logs.removed') : ''}
        </p>
        {attempts.isError && <div role="alert" className="space-y-2">
          <ErrorCard title={t('logs.loadFailed')} error={attempts.error} />
          {attempts.data !== undefined && <p className="text-sm text-muted">{t('logs.staleList')}</p>}
          <Button className="min-h-11" variant="secondary" disabled={attempts.isFetching} onClick={() => void attempts.refetch()}>{t('logs.retry')}</Button>
        </div>}
        {attempts.isSuccess && rows.length === 0 && <p>{t('logs.empty')}</p>}
        <ul className="space-y-3">
          {rows.map((a) => {
            const expanded = a.id === attemptId;
            return <li key={a.id} className={`min-w-0 rounded-lg border ${expanded ? 'border-accent' : 'border-border'}`}>
              <button type="button" id={`attempt-${a.id}`} aria-expanded={expanded} aria-controls={expanded ? `events-${a.id}` : undefined}
                onClick={() => { setSelected(expanded ? null : a.id); setRemoved(false); }}
                className="flex min-h-11 w-full items-start gap-3 rounded-lg p-3 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent">
                <div className="min-w-0 flex-1 space-y-1 break-words [overflow-wrap:anywhere]">
                  <p className="text-xs text-muted">{t('logs.issuedAt')}</p>
                  <p className="font-medium"><time dateTime={new Date(a.created_at * 1000).toISOString()}>{date(a.created_at)}</time></p>
                  <p className="text-sm">{label('os', a.os)} · {t('logs.operation')}</p>
                  <p className={`text-sm font-medium ${a.status === 'failed' ? 'text-danger' : a.status === 'succeeded' ? 'text-success' : 'text-muted'}`}>{label('status', a.status)}</p>
                  {a.last_stage && <p className="text-sm text-muted">{t('logs.lastReport')}: {label('stages', a.last_stage)}{a.last_outcome ? ` · ${label('outcomes', a.last_outcome)}` : ''}</p>}
                  {a.last_error_kind && <p className="text-sm text-danger">{t('logs.failedStage')}: {a.last_stage ? `${label('stages', a.last_stage)} · ` : ''}{label('errors', a.last_error_kind)}</p>}
                  {a.last_received_at != null && <p className="text-xs text-muted">{t('logs.lastReceivedAt')}: <time dateTime={new Date(a.last_received_at * 1000).toISOString()}>{date(a.last_received_at)}</time></p>}
                  {a.issuer?.trim() && <p className="text-xs text-muted">{t('logs.issuer')}: {a.issuer}</p>}
                </div>
                <ChevronDown aria-hidden="true" className={`mt-1 size-5 shrink-0 ${expanded ? 'rotate-180' : ''}`} />
              </button>
              {expanded && <section id={`events-${a.id}`} aria-labelledby={`attempt-${a.id}`} className="min-w-0 space-y-4 border-t border-border p-3">
                <p className="text-sm text-muted">{t('logs.registrationUnconfirmed')}</p>
                {a.status === 'unknown' && <p className="text-sm text-muted">{t('logs.unknownNote')}</p>}
                <p className="text-sm text-muted">{t('logs.expiresAt')}: <time dateTime={new Date(a.expires_at * 1000).toISOString()}>{date(a.expires_at)}</time></p>
                <p className="text-xs text-muted">{t('logs.receivedAtNote')}</p>
                <div className="space-y-1">
                  <p className="block text-xs text-muted" id={`id-${a.id}`}>{t('logs.attemptId')}</p>
                  <code aria-labelledby={`id-${a.id}`} className="block select-all break-all text-xs">{a.id}</code>
                  <Button className="min-h-11" variant="secondary" onClick={() => void copyId(a.id)}><Copy aria-hidden="true" className="size-4" />{t('logs.copyId')}</Button>
                </div>
                <p role="status" className="text-sm text-muted">{events.isPending ? t('logs.loadingEvents') : ''}</p>
                {events.isError && <div role="alert" className="space-y-2">
                  <ErrorCard title={t('logs.eventsFailed')} error={events.error} />
                  {events.data !== undefined && <p className="text-sm text-muted">{t('logs.staleEvents')}</p>}
                  <Button className="min-h-11" variant="secondary" disabled={events.isFetching} onClick={() => void events.refetch()}>{t('logs.retry')}</Button>
                </div>}
                {events.isSuccess && events.data.length === 0 && <p className="text-sm">{t('logs.noEvents')}</p>}
                <ol className="space-y-3">
                  {sortEvents(events.data ?? []).map((e) => <li key={e.seq} className="min-w-0 space-y-1 border-l-2 border-border pl-3 text-sm [overflow-wrap:anywhere]">
                    <p className="font-medium">{label('stages', e.stage)} · {label('outcomes', e.outcome)}</p>
                    <p className="text-xs text-muted">{t('logs.receivedAt')}: <time dateTime={new Date(e.received_at * 1000).toISOString()}>{date(e.received_at)}</time></p>
                    {e.error_kind && <p className="text-danger">{label('errors', e.error_kind)}</p>}
                    <details>
                      <summary className="min-h-11 cursor-pointer py-3 text-xs text-muted">{t('logs.technical')}</summary>
                      <dl className="space-y-1 break-all text-xs">
                        <dt>{t('logs.report')}</dt><dd>#{e.seq} · {e.stage} · {e.outcome}</dd>
                        <dt>{t('logs.exitCode')}</dt><dd>{e.exit_code}</dd>
                        {e.error_kind && <><dt>{t('logs.errorKind')}</dt><dd>{e.error_kind}</dd></>}
                        <dt>{t('logs.receivedAt')}</dt><dd>{new Date(e.received_at * 1000).toISOString()}</dd>
                      </dl>
                    </details>
                  </li>)}
                </ol>
              </section>}
            </li>;
          })}
        </ul>
      </CardContent>
    </Card>
  );
}
