import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ArrowLeft, Loader2, Pencil, Repeat, Trash2 } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { NotificationMarkdown } from '@/components/NotificationMarkdown';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
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
  EditNotificationRequest,
  NotificationDetail as NotificationDetailData,
  NotificationPriority,
} from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

/** ISO instant → local wall-clock "YYYY-MM-DDTHH:mm" for a datetime-local input. */
function isoToLocalInput(iso?: string | null): string {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const pad = (x: number) => String(x).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

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
  const queryClient = useQueryClient();
  const confirm = useConfirm();
  const { hasRole } = useAuth();
  const canOperate = hasRole('operator');

  const { data, error, isLoading } = useQuery({
    queryKey: ['notif-detail', id],
    queryFn: () =>
      apiFetch<NotificationDetailData>(`/api/notifications/${encodeURIComponent(id!)}`),
    enabled: !!id,
  });

  // Recall (完全削除): the notification is deleted from the stream, so on
  // success it's gone from the history list AND this detail page 404s — send
  // the operator back to the list rather than re-rendering a dead page.
  const recall = useMutation({
    mutationFn: () =>
      apiFetch<void>(`/api/notifications/${encodeURIComponent(id!)}/recall`, { method: 'POST' }),
    onSuccess: () => {
      toast.success(t('recall.done'));
      void queryClient.invalidateQueries({ queryKey: ['notif-history'] });
      void queryClient.removeQueries({ queryKey: ['notif-detail', id] });
      navigate('/notifications');
    },
    onError: (e) => toast.error(formatError(e)),
  });

  const onRecall = async () => {
    const ok = await confirm({
      title: t('recall.confirmTitle'),
      description: t('recall.confirmBody'),
      confirmLabel: t('recall.confirmLabel'),
      cancelLabel: t('recall.cancelLabel'),
      danger: true,
    });
    if (ok) recall.mutate();
  };

  // Edit (在席編集): re-publish the notification's content in place. The
  // audience is immutable, so there's no target picker — only the editable
  // fields + a "reset confirmations" toggle for a materially-changed body.
  const [editOpen, setEditOpen] = useState(false);
  const [ePriority, setEPriority] = useState<NotificationPriority>('info');
  const [eTitle, setETitle] = useState('');
  const [eBody, setEBody] = useState('');
  const [eRequireAck, setERequireAck] = useState(false);
  const [eToast, setEToast] = useState(false);
  const [eExpiresAt, setEExpiresAt] = useState('');
  const [eResetAcks, setEResetAcks] = useState(false);

  const edit = useMutation({
    mutationFn: (req: EditNotificationRequest) =>
      apiFetch<{ id: string; subjects: string[] }>(
        `/api/notifications/${encodeURIComponent(id!)}`,
        { method: 'PATCH', body: JSON.stringify(req) },
      ),
    onSuccess: () => {
      toast.success(t('edit.done'));
      setEditOpen(false);
      void queryClient.invalidateQueries({ queryKey: ['notif-detail', id] });
      void queryClient.invalidateQueries({ queryKey: ['notif-history'] });
    },
    onError: (e) => toast.error(formatError(e)),
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

  const openEdit = () => {
    setEPriority(n.priority);
    setETitle(n.title);
    setEBody(n.body);
    setERequireAck(n.require_ack);
    setEToast(n.toast);
    setEExpiresAt(isoToLocalInput(n.expires_at));
    setEResetAcks(false);
    setEditOpen(true);
  };

  const onSubmitEdit = () => {
    if (!eTitle.trim()) {
      toast.error(t('edit.titleRequired'));
      return;
    }
    const req: EditNotificationRequest = {
      priority: ePriority,
      require_ack: eRequireAck,
      toast: eToast,
      title: eTitle.trim(),
      body: eBody,
      reset_acks: eResetAcks,
    };
    // Empty field ⇒ omit ⇒ never expires (clears any prior expiry). A
    // malformed value aborts rather than silently dropping the expiry.
    if (eExpiresAt) {
      const parsed = new Date(eExpiresAt);
      if (Number.isNaN(parsed.getTime())) {
        toast.error(t('edit.invalidExpiresAt'));
        return;
      }
      req.expires_at = parsed.toISOString();
    }
    edit.mutate(req);
  };

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
        <div className="flex items-center gap-2">
          <Button variant="secondary" size="sm" onClick={onReuse}>
            <Repeat className="size-4" />
            {t('detail.reuse')}
          </Button>
          {canOperate && (
            <Button
              variant="secondary"
              size="sm"
              onClick={openEdit}
              title={t('edit.tooltip')}
            >
              <Pencil className="size-4" />
              {t('edit.action')}
            </Button>
          )}
          {canOperate && (
            <Button
              variant="danger"
              size="sm"
              onClick={onRecall}
              disabled={recall.isPending}
              title={t('recall.tooltip')}
            >
              {recall.isPending ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <Trash2 className="size-4" />
              )}
              {t('recall.action')}
            </Button>
          )}
        </div>
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
            {n.edited_at && (
              <span
                className="text-xs text-muted font-normal"
                title={t('edit.editedAt', { time: fmtIsoLocal(n.edited_at) })}
              >
                ({t('edit.editedBadge')})
              </span>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          {n.body ? (
            <NotificationMarkdown body={n.body} className="bg-muted/5 p-3 rounded" />
          ) : (
            <p className="text-muted text-sm">{t('detail.emptyBody')}</p>
          )}
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

      {/* ---- edit dialog (在席編集) ---- */}
      <Dialog open={editOpen} onOpenChange={(o) => !o && setEditOpen(false)}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('edit.dialogTitle')}</DialogTitle>
          </DialogHeader>
          <div className="space-y-4">
            <div className="space-y-1">
              <Label htmlFor="edit-priority">{t('compose.priority')}</Label>
              <Select
                id="edit-priority"
                value={ePriority}
                onChange={(e) => setEPriority(e.target.value as NotificationPriority)}
              >
                <option value="info">{t('priority.info')}</option>
                <option value="warn">{t('priority.warn')}</option>
                <option value="emergency">{t('priority.emergency')}</option>
              </Select>
            </div>
            <div className="space-y-1">
              <Label htmlFor="edit-title">{t('compose.titleField')}</Label>
              <Input id="edit-title" value={eTitle} onChange={(e) => setETitle(e.target.value)} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="edit-body">{t('compose.body')}</Label>
              <Textarea id="edit-body" value={eBody} onChange={(e) => setEBody(e.target.value)} />
              <p className="text-xs text-muted">{t('compose.markdownHint')}</p>
            </div>
            <div className="space-y-1">
              <Label>{t('compose.preview')}</Label>
              {eBody.trim() ? (
                <NotificationMarkdown
                  body={eBody}
                  className="rounded border border-border bg-muted/5 p-3"
                />
              ) : (
                <p className="text-xs text-muted">{t('compose.previewEmpty')}</p>
              )}
            </div>
            <div className="space-y-1">
              <Label htmlFor="edit-expires">{t('compose.expiresAt')}</Label>
              <Input
                id="edit-expires"
                type="datetime-local"
                value={eExpiresAt}
                onChange={(e) => setEExpiresAt(e.target.value)}
              />
              <p className="text-xs text-muted">{t('edit.expiresHint')}</p>
            </div>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={eRequireAck}
                onChange={(e) => setERequireAck(e.target.checked)}
                className="size-4 rounded border-border accent-violet"
              />
              {t('compose.requireAck')}
            </label>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={eToast}
                onChange={(e) => setEToast(e.target.checked)}
                className="size-4 rounded border-border accent-violet"
              />
              {t('compose.toast')}
            </label>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={eResetAcks}
                onChange={(e) => setEResetAcks(e.target.checked)}
                className="size-4 rounded border-border accent-violet"
              />
              {t('edit.resetAcks')}
            </label>
            <p className="text-xs text-muted -mt-2">{t('edit.resetAcksHint')}</p>
          </div>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setEditOpen(false)}>
              {t('edit.cancel')}
            </Button>
            <Button disabled={edit.isPending || !eTitle.trim()} onClick={onSubmitEdit}>
              {edit.isPending && <Loader2 className="size-4 animate-spin" />}
              {t('edit.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
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
