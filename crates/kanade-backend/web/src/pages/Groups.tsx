import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Check, Mail, Plus, Settings2, X } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router-dom';
import { toast } from 'sonner';

import { PcPicker } from '@/components/PcPicker';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';

// Mirror of the backend GroupSummary (api/agent_groups.rs): the
// group-centric inverse of the per-PC agent_groups KV rows, plus
// whether a `groups.<name>` config override exists and the group's
// notification email addresses (`group_contacts` KV).
type GroupSummary = {
  name: string;
  members: string[];
  has_config: boolean;
  emails: string[];
};

// Split an operator's free-form input (comma / whitespace / newline
// separated) into trimmed, non-empty address tokens. The backend
// re-normalises (lower-case + dedup + validate), so this is just a
// liberal tokenizer.
function parseEmails(raw: string): string[] {
  return raw
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

type GroupsOverview = {
  groups: GroupSummary[];
};

export function Groups() {
  const { t } = useTranslation('groups');
  const { hasRole } = useAuth();
  const canOperate = hasRole('operator');
  const qc = useQueryClient();
  const confirm = useConfirm();

  const overview = useQuery({
    queryKey: ['groups'],
    queryFn: () => apiFetch<GroupsOverview>('/api/groups'),
  });

  // add-membership form
  const [newGroup, setNewGroup] = useState('');
  const [newPcs, setNewPcs] = useState<string[]>([]);

  // Inline notify-email editor: which group's address list is open,
  // and the in-progress draft text.
  const [editingEmail, setEditingEmail] = useState<string | null>(null);
  const [emailDraft, setEmailDraft] = useState('');

  const invalidate = () => qc.invalidateQueries({ queryKey: ['groups'] });
  const onError = (err: unknown) => toast.error(formatError(err));

  // Each membership is a separate per-PC KV row (`POST
  // /api/agents/<pc>/groups`), so adding several at once is N
  // independent POSTs. Fired in batches (not one big Promise.all):
  // selecting 50-100 PCs would otherwise exceed the browser's per-host
  // connection cap and pile a burst onto the backend KV store. A
  // bounded window keeps it responsive without serialising every call.
  const add = useMutation({
    mutationFn: async (v: { pcIds: string[]; group: string }) => {
      const BATCH = 10;
      for (let i = 0; i < v.pcIds.length; i += BATCH) {
        await Promise.all(
          v.pcIds.slice(i, i + BATCH).map((pcId) =>
            apiFetch(`/api/agents/${encodeURIComponent(pcId)}/groups`, {
              method: 'POST',
              body: JSON.stringify({ group: v.group }),
            }),
          ),
        );
      }
    },
    onSuccess: (_data, v) => {
      toast.success(t('toast.added', { count: v.pcIds.length, group: v.group }));
      setNewPcs([]);
      invalidate();
    },
    onError: (err) => {
      onError(err);
      // Partial failure may have added some — refetch the real state.
      invalidate();
    },
  });

  const remove = useMutation({
    mutationFn: (v: { pcId: string; group: string }) =>
      apiFetch(
        `/api/agents/${encodeURIComponent(v.pcId)}/groups/${encodeURIComponent(v.group)}`,
        { method: 'DELETE' },
      ),
    onSuccess: (_data, v) => {
      toast.success(t('toast.removed', { pcId: v.pcId, group: v.group }));
      invalidate();
    },
    onError,
  });

  // "Empty this group" = remove it from every member. There is no
  // group entity to delete server-side — a group with zero members
  // and no config simply stops appearing in the overview. Each
  // DELETE touches a different per-PC KV row, so firing them in
  // parallel is safe (no read-modify-write contention).
  const removeAll = useMutation({
    mutationFn: async (g: GroupSummary) => {
      await Promise.all(
        g.members.map((pcId) =>
          apiFetch(
            `/api/agents/${encodeURIComponent(pcId)}/groups/${encodeURIComponent(g.name)}`,
            { method: 'DELETE' },
          ),
        ),
      );
    },
    onSuccess: (_data, g) => {
      toast.success(t('toast.removedAll', { group: g.name }));
      invalidate();
    },
    onError: (err) => {
      onError(err);
      // Partial failure leaves some members removed — refetch so the
      // table shows the actual remaining membership.
      invalidate();
    },
  });

  // Replace a group's notify-email list (`PUT /api/groups/<name>/email`).
  // The backend normalises + validates, so a bad address comes back as a
  // 400 surfaced via onError.
  const saveEmails = useMutation({
    mutationFn: (v: { group: string; emails: string[] }) =>
      apiFetch(`/api/groups/${encodeURIComponent(v.group)}/email`, {
        method: 'PUT',
        body: JSON.stringify({ emails: v.emails }),
      }),
    onSuccess: (_data, v) => {
      toast.success(t('toast.emailSaved', { group: v.group }));
      setEditingEmail(null);
      invalidate();
    },
    onError,
  });

  const groups = overview.data?.groups ?? [];

  return (
    <div className="p-4 md:p-6 space-y-6 max-w-5xl">
      <header>
        <h1 className="text-2xl font-bold">{t('title')}</h1>
        <p className="text-muted text-sm">{t('subtitle')}</p>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('addTitle')}</CardTitle>
        </CardHeader>
        <CardContent>
          <form
            className="flex flex-wrap items-end gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              const group = newGroup.trim();
              if (!group || newPcs.length === 0) return;
              add.mutate({ pcIds: newPcs, group });
            }}
          >
            <div className="space-y-1">
              <Label htmlFor="add-group">{t('groupName')}</Label>
              <Input
                id="add-group"
                value={newGroup}
                onChange={(e) => setNewGroup(e.target.value)}
                placeholder={t('groupNamePlaceholder')}
                list="group-names"
                className="w-44"
              />
              {/* Offer existing names as completions so "add another
                  PC to wave1" doesn't depend on retyping it exactly,
                  while still allowing a brand-new name. */}
              <datalist id="group-names">
                {groups.map((g) => (
                  <option key={g.name} value={g.name} />
                ))}
              </datalist>
            </div>
            <div className="space-y-1">
              <Label htmlFor="add-pc">{t('pc')}</Label>
              <PcPicker
                id="add-pc"
                mode="multi"
                value={newPcs}
                onChange={setNewPcs}
                placeholder={t('pcMultiPlaceholder')}
                className="w-64"
              />
            </div>
            <Button
              type="submit"
              disabled={!canOperate || !newGroup.trim() || newPcs.length === 0 || add.isPending}
              title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
            >
              <Plus className="size-4 mr-2" />
              {t('add')}
            </Button>
          </form>
          {!canOperate && (
            <p className="text-xs text-muted mt-2">
              {t('rbac.operatorRequired', { ns: 'common' })}
            </p>
          )}
        </CardContent>
      </Card>

      {overview.isError && (
        <p className="text-red-500 text-sm">{formatError(overview.error)}</p>
      )}

      {!overview.isLoading && groups.length === 0 ? (
        <p className="text-muted text-sm">{t('empty')}</p>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>{t('groupName')}</TableHead>
              <TableHead>{t('members')}</TableHead>
              <TableHead>{t('email')}</TableHead>
              <TableHead>{t('config')}</TableHead>
              <TableHead className="text-right">{t('actions')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {groups.map((g) => (
              <TableRow key={g.name}>
                <TableCell className="font-medium">
                  <code>{g.name}</code>
                </TableCell>
                <TableCell>
                  {g.members.length === 0 ? (
                    <span className="text-muted text-xs">{t('noMembers')}</span>
                  ) : (
                    <div className="flex flex-wrap gap-1.5">
                      {g.members.map((pc) => (
                        <span
                          key={pc}
                          className="inline-flex items-center gap-1 rounded bg-muted/10 px-1.5 py-0.5"
                        >
                          <Link to={`/agents/${encodeURIComponent(pc)}`}>
                            <code className="text-xs hover:underline">{pc}</code>
                          </Link>
                          {canOperate && (
                            <button
                              type="button"
                              aria-label={t('removeMember', { pcId: pc, group: g.name })}
                              disabled={remove.isPending || removeAll.isPending}
                              onClick={async () => {
                                if (
                                  await confirm({
                                    title: t('confirmRemove', { pcId: pc, group: g.name }),
                                    confirmLabel: t('remove'),
                                    danger: true,
                                  })
                                ) {
                                  remove.mutate({ pcId: pc, group: g.name });
                                }
                              }}
                              className="text-muted hover:text-fg"
                            >
                              <X className="size-3" />
                            </button>
                          )}
                        </span>
                      ))}
                    </div>
                  )}
                </TableCell>
                <TableCell>
                  {editingEmail === g.name ? (
                    <div className="flex items-center gap-1.5">
                      <Input
                        autoFocus
                        value={emailDraft}
                        onChange={(e) => setEmailDraft(e.target.value)}
                        placeholder={t('emailPlaceholder')}
                        title={t('emailHint')}
                        className="w-64"
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') {
                            e.preventDefault();
                            // Guard against a double-submit from holding/
                            // mashing Enter while the PUT is in flight.
                            if (!saveEmails.isPending) {
                              saveEmails.mutate({
                                group: g.name,
                                emails: parseEmails(emailDraft),
                              });
                            }
                          } else if (e.key === 'Escape') {
                            setEditingEmail(null);
                          }
                        }}
                      />
                      <Button
                        size="sm"
                        disabled={saveEmails.isPending}
                        aria-label={t('save')}
                        onClick={() =>
                          saveEmails.mutate({ group: g.name, emails: parseEmails(emailDraft) })
                        }
                      >
                        <Check className="size-3" />
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        aria-label={t('cancel')}
                        onClick={() => setEditingEmail(null)}
                      >
                        <X className="size-3" />
                      </Button>
                    </div>
                  ) : (
                    <button
                      type="button"
                      disabled={!canOperate}
                      title={canOperate ? t('emailEdit', { group: g.name }) : undefined}
                      onClick={() => {
                        setEditingEmail(g.name);
                        setEmailDraft(g.emails.join(', '));
                      }}
                      className="inline-flex items-center gap-1 text-left enabled:hover:text-fg disabled:cursor-default"
                    >
                      <Mail className="size-3 text-muted shrink-0" />
                      {g.emails.length === 0 ? (
                        <span className="text-muted text-xs">{t('emailNone')}</span>
                      ) : (
                        <span className="text-xs break-all">{g.emails.join(', ')}</span>
                      )}
                    </button>
                  )}
                </TableCell>
                <TableCell>
                  {g.has_config ? (
                    <Link to="/config" title={t('configHint')}>
                      <Badge variant="violet">
                        <Settings2 className="size-3 mr-1" />
                        {t('configured')}
                      </Badge>
                    </Link>
                  ) : (
                    <span className="text-muted text-xs">—</span>
                  )}
                </TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  {canOperate && g.members.length > 0 && (
                    <Button
                      variant="danger"
                      size="sm"
                      disabled={removeAll.isPending}
                      onClick={async () => {
                        if (
                          await confirm({
                            title: t('confirmRemoveAll', {
                              group: g.name,
                              count: g.members.length,
                            }),
                            confirmLabel: t('removeAllLabel'),
                            danger: true,
                          })
                        ) {
                          removeAll.mutate(g);
                        }
                      }}
                    >
                      {t('removeAll')}
                    </Button>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}
