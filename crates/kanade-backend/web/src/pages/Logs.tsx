import { useQuery } from '@tanstack/react-query';
import { Loader2, RefreshCw } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { PcPicker } from '@/components/PcPicker';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';

export function Logs() {
  const { t } = useTranslation('logs');
  const [search, setSearch] = useSearchParams();
  const initialPc = search.get('pc') ?? '';
  const [pcId, setPcId] = useState(initialPc);
  const [tail, setTail] = useState(500);
  const [autoRefresh, setAutoRefresh] = useState(false);
  const preRef = useRef<HTMLPreElement>(null);

  // Keep ?pc=… in the URL so the page is shareable / reload-safe.
  useEffect(() => {
    if (pcId) {
      setSearch({ pc: pcId }, { replace: true });
    } else if (search.has('pc')) {
      const next = new URLSearchParams(search);
      next.delete('pc');
      setSearch(next, { replace: true });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pcId]);

  const logsQ = useQuery({
    enabled: !!pcId,
    queryKey: ['agent-logs', pcId, tail],
    queryFn: async () => {
      const res = await fetch(`/api/agents/${encodeURIComponent(pcId)}/logs?tail=${tail}`, {
        headers: authHeaders(),
      });
      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText} — ${await res.text()}`);
      }
      return res.text();
    },
    refetchInterval: autoRefresh ? 5_000 : false,
  });

  // Auto-scroll to bottom on new log content.
  useEffect(() => {
    if (logsQ.data && preRef.current) {
      preRef.current.scrollTop = preRef.current.scrollHeight;
    }
  }, [logsQ.data]);

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        {pcId && (
          <Badge variant="violet">
            {logsQ.data
              ? t('countBadge', { count: logsQ.data.split('\n').length - 1 })
              : t('countBadgeEmpty')}
          </Badge>
        )}
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-4 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="logs-pc">{t('filters.pcId')}</Label>
            <PcPicker id="logs-pc" value={pcId} onChange={setPcId} />
          </div>
          <div className="space-y-1">
            <Label htmlFor="logs-tail">{t('filters.tail')}</Label>
            <Input
              id="logs-tail"
              type="number"
              min={1}
              max={5000}
              value={tail}
              onChange={(e) => setTail(Math.max(1, Number(e.target.value) || 500))}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="logs-auto">{t('filters.autoRefresh')}</Label>
            <Select
              id="logs-auto"
              value={autoRefresh ? 'on' : 'off'}
              onChange={(e) => setAutoRefresh(e.target.value === 'on')}
            >
              <option value="off">{t('filters.autoRefreshOptions.off')}</option>
              <option value="on">{t('filters.autoRefreshOptions.on')}</option>
            </Select>
          </div>
          <div className="space-y-1">
            <Label>&nbsp;</Label>
            <Button
              variant="secondary"
              onClick={() => logsQ.refetch()}
              disabled={!pcId || logsQ.isFetching}
              className="w-full"
            >
              <RefreshCw className={logsQ.isFetching ? 'size-4 mr-2 animate-spin' : 'size-4 mr-2'} />
              {t('actions.refresh')}
            </Button>
          </div>
        </CardContent>
      </Card>

      {!pcId ? (
        <Card>
          <CardContent className="p-6 text-muted text-sm">
            <Trans ns="logs" i18nKey="empty.intro" components={{ code: <code /> }} />
            {' '}
            {t('empty.timeout')}
          </CardContent>
        </Card>
      ) : logsQ.isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />
          {t('loading', { pcId })}
        </div>
      ) : logsQ.error ? (
        <ErrorCard title={t('errorTitle', { pcId })} error={logsQ.error} />
      ) : (
        <pre
          ref={preRef}
          className="text-xs whitespace-pre-wrap break-words bg-muted/5 border border-border rounded p-3 max-h-[70vh] overflow-auto font-mono"
        >
          {logsQ.data || t('empty.pre')}
        </pre>
      )}
    </div>
  );
}

function authHeaders(): HeadersInit {
  const token = localStorage.getItem('kanade_token') ?? '';
  return token ? { Authorization: `Bearer ${token}` } : {};
}
