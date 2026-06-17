import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  AlertTriangle,
  ExternalLink,
  History,
  Loader2,
  RefreshCw,
  Repeat,
  Send,
} from 'lucide-react';
import { type FormEvent, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation, useNavigate } from 'react-router-dom';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { PcPicker } from '@/components/PcPicker';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Textarea } from '@/components/ui/textarea';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import type {
  NotificationAckStatus,
  NotificationPriority,
  NotificationRecord,
  NotificationReuse,
  PublishNotificationRequest,
  PublishNotificationResponse,
} from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

type TargetMode = 'all' | 'groups' | 'pcs';

function splitCsv(s: string): string[] {
  // Dedup: the backend fans a notification out once per group entry, so
  // `finance, finance` would otherwise deliver the same message twice.
  return [
    ...new Set(
      s
        .split(',')
        .map((x) => x.trim())
        .filter(Boolean),
    ),
  ];
}

export function Notifications() {
  const { t } = useTranslation('notifications');
  const { hasRole } = useAuth();
  const canOperate = hasRole('operator');
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const location = useLocation();

  // ---- composer state ----
  const [priority, setPriority] = useState<NotificationPriority>('info');
  const [requireAck, setRequireAck] = useState(false);
  const [title, setTitle] = useState('');
  const [body, setBody] = useState('');
  const [issuedBy, setIssuedBy] = useState('');
  const [expiresAt, setExpiresAt] = useState('');
  const [mode, setMode] = useState<TargetMode>('pcs');
  const [groups, setGroups] = useState('');
  const [pcs, setPcs] = useState<string[]>([]);

  // ---- ack-status state ----
  // `ackId` is the free-text input; `ackQueryId` is what the query
  // actually keys off. They are decoupled so typing/pasting an id does
  // NOT fire a request per keystroke (global staleTime is 0) — the
  // query only runs once the operator submits, or once a publish
  // auto-seeds both from its response.
  const [ackId, setAckId] = useState('');
  const [ackQueryId, setAckQueryId] = useState('');

  const publish = useMutation({
    mutationFn: (req: PublishNotificationRequest) =>
      apiFetch<PublishNotificationResponse>('/api/notifications', {
        method: 'POST',
        body: JSON.stringify(req),
      }),
    onSuccess: (data) => {
      toast.success(t('toast.published', { id: data.id, count: data.subjects.length }));
      setAckId(data.id);
      setAckQueryId(data.id);
      // Keep the composed message on screen (operators often send a
      // follow-up to a different target) but clear the one-shot fields.
      setTitle('');
      setBody('');
      // Surface the just-sent notification in the history list without a
      // manual refresh.
      void queryClient.invalidateQueries({ queryKey: ['notif-history'] });
    },
    onError: (e) => toast.error(formatError(e)),
  });

  const ack = useQuery({
    queryKey: ['notif-ack', ackQueryId],
    queryFn: () =>
      apiFetch<NotificationAckStatus>(
        `/api/notifications/${encodeURIComponent(ackQueryId)}/ack_status`,
      ),
    enabled: ackQueryId.trim().length > 0,
  });

  // ---- sent history ----
  const history = useQuery({
    queryKey: ['notif-history'],
    queryFn: () => apiFetch<NotificationRecord[]>('/api/notifications'),
    // The fetch replays the whole NOTIFICATIONS stream (up to ~10 NATS
    // round-trips), so don't re-run it on every window refocus — a short
    // stale window covers the typical tab-switch-and-back.
    staleTime: 30_000,
  });

  // "Reuse" a sent notification: a history row / the detail page navigates
  // here carrying the prior content in router state, and we seed the
  // composer with it for a quick edit-and-resend. Applied once, then the
  // state is cleared (navigate-replace) so a refresh doesn't re-prefill
  // over edits the operator has since made. The audience is NOT stored on
  // the notification, so the target picker stays at its default — the
  // operator re-picks who to send to.
  useEffect(() => {
    const reuse = (location.state as NotificationReuse | null)?.reuse;
    if (!reuse) return;
    setPriority(reuse.priority);
    setRequireAck(reuse.require_ack);
    setTitle(reuse.title);
    setBody(reuse.body);
    setIssuedBy(reuse.issued_by ?? '');
    navigate(location.pathname, { replace: true, state: null });
  }, [location.state, location.pathname, navigate]);

  const targetReady =
    mode === 'all'
    || (mode === 'groups' && splitCsv(groups).length > 0)
    || (mode === 'pcs' && pcs.length > 0);
  const canSubmit = canOperate && title.trim().length > 0 && targetReady && !publish.isPending;

  const onPublish = (e: FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    const req: PublishNotificationRequest = {
      priority,
      require_ack: requireAck,
      title: title.trim(),
      body,
      target: {
        all: mode === 'all',
        groups: mode === 'groups' ? splitCsv(groups) : [],
        pcs: mode === 'pcs' ? pcs : [],
      },
    };
    if (issuedBy.trim()) req.issued_by = issuedBy.trim();
    // <input type="datetime-local"> yields wall-clock "YYYY-MM-DDTHH:mm"
    // with no zone; convert to RFC3339 (UTC) for the backend. Abort (not
    // silently drop) on a malformed value — silently dropping it would
    // turn an operator typo into a non-expiring notification.
    if (expiresAt) {
      const parsed = new Date(expiresAt);
      if (Number.isNaN(parsed.getTime())) {
        toast.error(t('compose.invalidExpiresAt'));
        return;
      }
      req.expires_at = parsed.toISOString();
    }
    publish.mutate(req);
  };

  return (
    <div className="p-4 md:p-6 space-y-6 max-w-5xl">
      <header>
        <h1 className="text-2xl font-bold">{t('title')}</h1>
        <p className="text-muted text-sm">{t('subtitle')}</p>
      </header>

      {/* ---- composer ---- */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('compose.title')}</CardTitle>
        </CardHeader>
        <CardContent>
          <form className="space-y-4" onSubmit={onPublish}>
            <div className="grid gap-4 sm:grid-cols-2">
              <div className="space-y-1">
                <Label htmlFor="notif-priority">{t('compose.priority')}</Label>
                <Select
                  id="notif-priority"
                  value={priority}
                  onChange={(e) => setPriority(e.target.value as NotificationPriority)}
                >
                  <option value="info">{t('priority.info')}</option>
                  <option value="warn">{t('priority.warn')}</option>
                  <option value="emergency">{t('priority.emergency')}</option>
                </Select>
              </div>
              <div className="space-y-1">
                <Label htmlFor="notif-issuedby">{t('compose.issuedBy')}</Label>
                <Input
                  id="notif-issuedby"
                  value={issuedBy}
                  onChange={(e) => setIssuedBy(e.target.value)}
                  placeholder={t('compose.issuedByPlaceholder')}
                />
              </div>
            </div>

            <div className="space-y-1">
              <Label htmlFor="notif-title">{t('compose.titleField')}</Label>
              <Input
                id="notif-title"
                value={title}
                onChange={(e) => setTitle(e.target.value)}
                placeholder={t('compose.titlePlaceholder')}
              />
            </div>

            <div className="space-y-1">
              <Label htmlFor="notif-body">{t('compose.body')}</Label>
              <Textarea
                id="notif-body"
                value={body}
                onChange={(e) => setBody(e.target.value)}
                placeholder={t('compose.bodyPlaceholder')}
              />
            </div>

            <div className="grid gap-4 sm:grid-cols-2">
              <div className="space-y-1">
                <Label htmlFor="notif-target">{t('compose.target')}</Label>
                <Select
                  id="notif-target"
                  value={mode}
                  onChange={(e) => setMode(e.target.value as TargetMode)}
                >
                  <option value="all">{t('target.all')}</option>
                  <option value="groups">{t('target.groups')}</option>
                  <option value="pcs">{t('target.pcs')}</option>
                </Select>
              </div>
              <div className="space-y-1">
                <Label htmlFor="notif-expires">{t('compose.expiresAt')}</Label>
                <Input
                  id="notif-expires"
                  type="datetime-local"
                  value={expiresAt}
                  onChange={(e) => setExpiresAt(e.target.value)}
                />
              </div>
            </div>

            {mode === 'all' && (
              <div
                role="alert"
                className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 p-2 text-sm text-danger"
              >
                <AlertTriangle className="size-4 mt-0.5 shrink-0" />
                <span>{t('compose.allWarning')}</span>
              </div>
            )}
            {mode === 'groups' && (
              <div className="space-y-1">
                <Label htmlFor="notif-groups">{t('compose.groups')}</Label>
                <Input
                  id="notif-groups"
                  value={groups}
                  onChange={(e) => setGroups(e.target.value)}
                  placeholder={t('compose.groupsPlaceholder')}
                />
              </div>
            )}
            {mode === 'pcs' && (
              <div className="space-y-1">
                <Label htmlFor="notif-pcs">{t('compose.pcs')}</Label>
                <PcPicker mode="multi" id="notif-pcs" value={pcs} onChange={setPcs} />
              </div>
            )}

            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={requireAck}
                onChange={(e) => setRequireAck(e.target.checked)}
                className="size-4 rounded border-border accent-violet"
              />
              {t('compose.requireAck')}
            </label>
            <p className="text-xs text-muted -mt-2">{t('compose.requireAckHint')}</p>

            <Button
              type="submit"
              disabled={!canSubmit}
              title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
            >
              {publish.isPending ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <Send className="size-4" />
              )}
              {t('compose.publish')}
            </Button>
            {!canOperate && (
              <p className="text-xs text-muted">{t('rbac.operatorRequired', { ns: 'common' })}</p>
            )}
          </form>
        </CardContent>
      </Card>

      {/* ---- sent history ---- */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-2">
          <CardTitle className="text-base flex items-center gap-2">
            <History className="size-4" />
            {t('history.title')}
          </CardTitle>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => history.refetch()}
            disabled={history.isFetching}
          >
            {history.isFetching ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <RefreshCw className="size-4" />
            )}
            {t('history.refresh')}
          </Button>
        </CardHeader>
        <CardContent className="space-y-3">
          {history.isLoading && (
            <p className="text-muted text-sm flex items-center gap-2">
              <Loader2 className="size-4 animate-spin" />
              {t('history.loading')}
            </p>
          )}
          {history.isError && <ErrorCard title={t('history.loadError')} error={history.error} />}
          {history.data &&
            (history.data.length === 0 ? (
              <p className="text-muted text-sm">{t('history.empty')}</p>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('history.priority')}</TableHead>
                    <TableHead>{t('history.titleCol')}</TableHead>
                    <TableHead>{t('history.issuedBy')}</TableHead>
                    <TableHead>{t('history.issuedAt')}</TableHead>
                    <TableHead className="text-right">{t('history.actions')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {history.data.map((n) => {
                    const exp = n.expires_at ? Date.parse(n.expires_at) : Number.NaN;
                    const expired = !Number.isNaN(exp) && exp <= Date.now();
                    return (
                      <TableRow key={n.id}>
                        <TableCell>{t(`priority.${n.priority}`)}</TableCell>
                        <TableCell className="font-medium">
                          {n.title}
                          {n.require_ack && (
                            <span className="ml-1 text-xs text-muted">
                              ({t('history.requireAck')})
                            </span>
                          )}
                          {expired && (
                            <span className="ml-1 text-xs text-danger">
                              ({t('history.expired')})
                            </span>
                          )}
                        </TableCell>
                        <TableCell className="text-muted">{n.issued_by ?? '—'}</TableCell>
                        <TableCell>{fmtIsoLocal(n.issued_at)}</TableCell>
                        <TableCell className="text-right">
                          <div className="flex items-center justify-end gap-1">
                            <Button
                              variant="ghost"
                              size="sm"
                              title={t('history.reuseTitle')}
                              onClick={() =>
                                navigate('/notifications', {
                                  state: {
                                    reuse: {
                                      priority: n.priority,
                                      require_ack: n.require_ack,
                                      title: n.title,
                                      body: n.body,
                                      issued_by: n.issued_by ?? null,
                                    },
                                  },
                                })
                              }
                            >
                              <Repeat className="size-3.5" />
                              {t('history.reuse')}
                            </Button>
                            {/* Deep-link to the detail page (content + ack
                                status) — Ctrl/⌘ click opens a new tab so the
                                confirmation status is right there instead of
                                scrolling to a card at the bottom. Mirrors the
                                Activity result_id link. */}
                            <Button variant="ghost" size="sm" asChild>
                              <Link
                                to={`/notifications/${encodeURIComponent(n.id)}`}
                                title={t('history.viewDetailTitle')}
                              >
                                {t('history.viewDetail')}
                                <ExternalLink className="size-3.5" />
                              </Link>
                            </Button>
                          </div>
                        </TableCell>
                      </TableRow>
                    );
                  })}
                </TableBody>
              </Table>
            ))}
        </CardContent>
      </Card>

      {/* ---- ack status ---- */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('ack.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <form
            className="flex flex-wrap items-end gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              const trimmed = ackId.trim();
              if (!trimmed) return;
              // Same id → key is unchanged, so refetch by hand; new id →
              // updating the query key triggers the fetch on its own.
              if (trimmed === ackQueryId) ack.refetch();
              else setAckQueryId(trimmed);
            }}
          >
            <div className="space-y-1 grow">
              <Label htmlFor="notif-ackid">{t('ack.idField')}</Label>
              <Input
                id="notif-ackid"
                value={ackId}
                onChange={(e) => setAckId(e.target.value)}
                placeholder={t('ack.idPlaceholder')}
                className="font-mono"
              />
            </div>
            <Button
              type="submit"
              variant="secondary"
              disabled={!ackId.trim() || ack.isFetching}
            >
              {ack.isFetching ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <RefreshCw className="size-4" />
              )}
              {t('ack.refresh')}
            </Button>
          </form>

          {ack.isError && <ErrorCard title={t('ack.loadError')} error={ack.error} />}

          {ack.data &&
            (ack.data.acks.length === 0 ? (
              <p className="text-muted text-sm">{t('ack.empty')}</p>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('ack.pc')}</TableHead>
                    <TableHead>{t('ack.user')}</TableHead>
                    <TableHead>{t('ack.ackedAt')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {ack.data.acks.map((a) => (
                    <TableRow key={`${a.pc_id}::${a.user_sid}`}>
                      <TableCell className="font-medium">
                        <code>{a.pc_id}</code>
                      </TableCell>
                      <TableCell>
                        {a.account?.trim() ? (
                          <span title={a.user_sid}>{a.account}</span>
                        ) : (
                          <code className="text-xs">{a.user_sid}</code>
                        )}
                      </TableCell>
                      <TableCell>{fmtIsoLocal(a.acked_at)}</TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            ))}
          {ack.data && ack.data.acks.length > 0 && (
            <p className="text-xs text-muted">
              {t('ack.count', { count: ack.data.acks.length })}
            </p>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
