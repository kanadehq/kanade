import { useQuery } from '@tanstack/react-query';
import { ArrowLeft, Loader2, Repeat } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate, useParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { NotificationDetail as NotificationDetailData, NotificationPriority } from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

/** Priority → Badge variant. info is neutral; warn/emergency escalate. */
function priorityVariant(p: NotificationPriority): 'default' | 'amber' | 'danger' {
  if (p === 'emergency') return 'danger';
  if (p === 'warn') return 'amber';
  return 'default';
}

/**
 * Per-notification detail page (`/notifications/{id}`). Mirrors the
 * Activity → result-detail deep link: each history row links here, and an
 * operator Ctrl/⌘-clicks to open it in a new tab so the confirmation
 * status is right there instead of scrolling to a shared card at the
 * bottom of the compose page.
 *
 * Shows the full sent content (including the `body` the history table
 * drops) so "what did I send" is answerable, plus the per-recipient
 * confirmation list. The "reuse" button seeds the composer with this
 * notification's content for a quick edit-and-resend (the audience is not
 * stored on the notification, so the operator re-picks the target).
 */
export function NotificationDetail() {
  const { t } = useTranslation('notifications');
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();

  const { data, error, isLoading } = useQuery({
    queryKey: ['notif-detail', id],
    queryFn: () =>
      apiFetch<NotificationDetailData>(`/api/notifications/${encodeURIComponent(id!)}`),
    enabled: !!id,
  });

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted p-4">
        <Loader2 className="size-4 animate-spin" />
        {t('detail.loading')}
      </div>
    );
  }
  if (error) return <ErrorCard title={t('detail.loadError')} error={error} />;
  if (!data) return <ErrorCard title={t('detail.notFound')} error={new Error(`${id}`)} />;

  const n = data.notification;
  const exp = n.expires_at ? Date.parse(n.expires_at) : Number.NaN;
  const expired = !Number.isNaN(exp) && exp <= Date.now();

  const onReuse = () => {
    navigate('/notifications', {
      state: {
        reuse: {
          priority: n.priority,
          require_ack: n.require_ack,
          toast: n.toast,
          title: n.title,
          body: n.body,
          issued_by: n.issued_by ?? null,
        },
      },
    });
  };

  return (
    <div className="p-4 md:p-6 space-y-4 max-w-5xl">
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-baseline gap-3">
          <Button variant="ghost" size="sm" asChild>
            <Link to="/notifications">
              <ArrowLeft className="size-3.5" />
              {t('detail.back')}
            </Link>
          </Button>
        </div>
        <Button variant="secondary" size="sm" onClick={onReuse}>
          <Repeat className="size-4" />
          {t('detail.reuse')}
        </Button>
      </div>

      {/* ---- what was sent (①) ---- */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base flex items-center gap-2">
            <Badge variant={priorityVariant(n.priority)}>{t(`priority.${n.priority}`)}</Badge>
            {n.title}
            {n.require_ack && (
              <span className="text-xs text-muted font-normal">({t('history.requireAck')})</span>
            )}
            {expired && (
              <span className="text-xs text-danger font-normal">({t('history.expired')})</span>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          <pre className="whitespace-pre-wrap break-words bg-muted/5 p-3 rounded text-sm">
            {n.body || t('detail.emptyBody')}
          </pre>
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-y-2 gap-x-6">
            <Field label={t('history.issuedBy')} value={n.issued_by || '—'} />
            <Field label={t('history.issuedAt')} value={fmtIsoLocal(n.issued_at)} />
            <Field
              label={t('compose.expiresAt')}
              value={n.expires_at ? fmtIsoLocal(n.expires_at) : '—'}
            />
            <Field label={t('detail.id')} value={<code className="text-xs">{n.id}</code>} />
          </div>
          {data.target && (
            <Field
              label={t('detail.target')}
              value={
                <span className="flex flex-wrap items-center gap-1">
                  {data.target.all && <Badge variant="violet">{t('target.all')}</Badge>}
                  {data.target.groups.map((g) => (
                    <Badge key={`g-${g}`} variant="default">
                      {t('detail.targetGroup', { name: g })}
                    </Badge>
                  ))}
                  {data.target.pcs.map((pc) => (
                    <Badge key={`p-${pc}`} variant="default">
                      {t('detail.targetPc', { name: pc })}
                    </Badge>
                  ))}
                  {!data.target.all &&
                    data.target.groups.length === 0 &&
                    data.target.pcs.length === 0 && (
                      <span className="text-muted text-xs">{t('detail.targetUnknown')}</span>
                    )}
                </span>
              }
            />
          )}
        </CardContent>
      </Card>

      {/* ---- audience roster: who hasn't confirmed (④) ---- */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t('audience.title')}
            {data.audience.length > 0 && (
              <span className="ml-2 text-xs text-muted font-normal">
                {t('audience.summary', {
                  confirmed: data.audience.filter((p) => p.confirmed).length,
                  total: data.audience.length,
                })}
              </span>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {data.audience.length === 0 ? (
            // Render the card even when empty so the section isn't silently
            // absent: an empty roster means the targeted PCs couldn't be
            // resolved (e.g. a group with no current members), which is
            // itself worth surfacing rather than hiding.
            <p className="text-muted text-sm">{t('audience.empty')}</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('audience.pc')}</TableHead>
                  <TableHead>{t('audience.user')}</TableHead>
                  <TableHead>{t('audience.status')}</TableHead>
                  <TableHead>{t('ack.ackedAt')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {data.audience.map((p) => {
                  // Representative user: display name preferred, else login.
                  const who = p.last_logon_display_name || p.last_logon_user;
                  return (
                    <TableRow key={p.pc_id}>
                      <TableCell className="font-medium">
                        <code>{p.pc_id}</code>
                      </TableCell>
                      <TableCell>
                        {who ? (
                          <span title={p.last_logon_user}>{who}</span>
                        ) : (
                          <span className="text-muted text-xs">—</span>
                        )}
                      </TableCell>
                      <TableCell>
                        {p.confirmed ? (
                          <Badge variant="success">{t('audience.confirmed')}</Badge>
                        ) : (
                          <Badge variant="amber">{t('audience.pending')}</Badge>
                        )}
                      </TableCell>
                      <TableCell>{p.acked_at ? fmtIsoLocal(p.acked_at) : '—'}</TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      {/* ---- confirmation status ---- */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('ack.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          {data.acks.length === 0 ? (
            <p className="text-muted text-sm">{t('ack.empty')}</p>
          ) : (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('ack.pc')}</TableHead>
                    <TableHead>{t('ack.user')}</TableHead>
                    <TableHead>{t('ack.ackedAt')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {data.acks.map((a) => (
                    <TableRow key={`${a.pc_id}::${a.user_sid}`}>
                      <TableCell className="font-medium">
                        <code>{a.pc_id}</code>
                      </TableCell>
                      <TableCell>
                        {a.account?.trim() ? (
                          // SID kept in a tooltip — the account label is what
                          // an operator actually recognises. `trim()` guards a
                          // whitespace-only label (truthy but blank on screen).
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
              <p className="text-xs text-muted">{t('ack.count', { count: data.acks.length })}</p>
            </>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

function Field({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex gap-3 items-baseline">
      <span className="text-muted text-xs uppercase tracking-wide w-24 shrink-0">{label}</span>
      <span>{value}</span>
    </div>
  );
}
