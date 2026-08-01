import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Check,
  ChevronDown,
  ChevronRight,
  Loader2,
  Mail,
  Pencil,
  Plus,
  Trash2,
  X,
} from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { type EditorMode, type RepoOrigin, YamlEditorDialog } from '@/components/YamlEditorDialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';

// Mirrors `kanade_shared::manifest::GroupDef` (the bits the list renders).
// `members` / `query` are omitted when empty/absent by the backend (serde
// skip), so both are optional here — a static group carries `members`, a
// dynamic one a `query`.
type GroupDefRow = {
  id: string;
  description?: string | null;
  members?: string[];
  query?: string | null;
  refresh?: string | null;
  tags?: string[];
  /** GitOps provenance (#678) — present ⇒ open the editor read-only. */
  origin?: RepoOrigin | null;
};

// `GET /api/group-defs/{id}/members` — the resolved pc_id set (a dynamic
// group runs its query server-side; a static group returns its literal list).
type GroupMembers = { id: string; kind: string; count: number; members: string[] };

// `GET /api/groups/{name}/email` — mirrors `kanade_shared::wire::GroupContacts`.
type GroupContacts = { emails: string[] };

function isDynamic(g: GroupDefRow): boolean {
  return typeof g.query === 'string' && g.query.trim().length > 0;
}

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

export function Groups() {
  const { t } = useTranslation('groups');
  const { hasRole } = useAuth();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const [editor, setEditor] = useState<EditorMode | null>(null);
  const [preview, setPreview] = useState<Set<string>>(new Set());
  // Self-gate the write controls for non-operators (the backend RBAC already
  // rejects the writes, but hiding the buttons stops a viewer hitting a 403;
  // matches the Compliance page's operator-gated clear button). Viewers keep
  // the read-only list + members preview.
  const canOperate = hasRole('operator');

  const { data, error, isLoading } = useQuery({
    queryKey: ['group-defs'],
    queryFn: () => apiFetch<GroupDefRow[]>('/api/group-defs'),
  });

  const del = useMutation({
    mutationFn: async (id: string) => {
      await apiFetch(`/api/group-defs/${encodeURIComponent(id)}`, { method: 'DELETE' });
      // Clear the group's notification addresses too (#1274). A group with no
      // definition has no row on this page any more, so contacts left behind
      // would keep routing compliance alerts to an address nothing can show
      // or edit. The PUT is idempotent and an empty list is filtered out of
      // every read path (`contacts_map`), so it is a no-op for a group that
      // never had contacts. Best-effort: a failure here must not report the
      // (already successful) delete as failed.
      try {
        await apiFetch(`/api/groups/${encodeURIComponent(id)}/email`, {
          method: 'PUT',
          body: JSON.stringify({ emails: [] }),
        });
      } catch (e) {
        toast.warning(t('contacts.clearFailed', { id, error: formatError(e) }));
      }
    },
    onSuccess: (_r, id) => {
      qc.invalidateQueries({ queryKey: ['group-defs'] });
      qc.invalidateQueries({ queryKey: ['group-contacts', id] });
      toast.success(t('toast.deleted', { id }));
    },
    onError: (e) => toast.error(t('toast.deleteFailed', { error: formatError(e) })),
  });

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl">{t('title')}</h2>
          <p className="text-muted text-sm">{t('subtitle')}</p>
        </div>
        {canOperate && (
          <Button onClick={() => setEditor({ type: 'create' })}>
            <Plus className="size-4" />
            {t('actions.new')}
          </Button>
        )}
      </div>

      {error ? (
        <ErrorCard title={t('errorTitle')} error={error} />
      ) : isLoading ? (
        <div className="text-muted text-sm">{t('loading')}</div>
      ) : !data || data.length === 0 ? (
        <Card>
          <CardContent className="py-8 text-center text-muted text-sm">{t('empty')}</CardContent>
        </Card>
      ) : (
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
          {data.map((g) => {
            const dynamic = isDynamic(g);
            const open = preview.has(g.id);
            return (
              <Card key={g.id}>
                <CardHeader className="flex flex-row items-start justify-between gap-2">
                  <div className="min-w-0">
                    <CardTitle className="text-sm flex items-center gap-2">
                      <code>{g.id}</code>
                      <Badge variant={dynamic ? 'violet' : 'default'}>
                        {dynamic ? t('kind.dynamic') : t('kind.static')}
                      </Badge>
                    </CardTitle>
                    {g.description && <p className="mt-1 text-muted text-xs">{g.description}</p>}
                  </div>
                  {canOperate && (
                    <div className="flex shrink-0 gap-1">
                      <Button
                        variant="secondary"
                        size="sm"
                        onClick={() => setEditor({ type: 'edit', id: g.id })}
                        aria-label={t('actions.editAria', { id: g.id })}
                      >
                        <Pencil className="size-3.5" />
                      </Button>
                      <Button
                        variant="secondary"
                        size="sm"
                        aria-label={t('actions.deleteAria', { id: g.id })}
                        onClick={async () => {
                          const ok = await confirm({
                            title: t('confirm.deleteTitle', { id: g.id }),
                            description: t('confirm.deleteDescription'),
                            confirmLabel: t('confirm.deleteLabel'),
                          });
                          if (ok) del.mutate(g.id);
                        }}
                      >
                        <Trash2 className="size-3.5 text-danger" />
                      </Button>
                    </div>
                  )}
                </CardHeader>
                <CardContent className="space-y-2">
                  {/* Definition summary: static shows its member count; dynamic
                      shows the recompute cadence + the query. */}
                  <div className="text-muted text-xs">
                    {dynamic
                      ? t('dynamicSummary', { refresh: g.refresh || t('defaultRefresh') })
                      : t('staticSummary', { count: g.members?.length ?? 0 })}
                  </div>
                  {dynamic && g.query && (
                    <pre className="max-h-40 overflow-auto rounded bg-muted/10 p-2 text-[11px] leading-snug">
                      {g.query.trim()}
                    </pre>
                  )}
                  {g.tags && g.tags.length > 0 && (
                    <div className="flex flex-wrap gap-1">
                      {g.tags.map((tag) => (
                        <Badge key={tag} variant="default">
                          {tag}
                        </Badge>
                      ))}
                    </div>
                  )}

                  {/* Notification addresses for this group (#1274). `group_contacts`
                      is keyed by group name, and a definition's id IS that name —
                      it is the string the materializer stores in each member PC's
                      `agent_groups_derived` row (that bucket is keyed by pc_id; the
                      group name lives in the value). So the compliance-alert
                      fan-out resolves exactly the key edited here. */}
                  <ContactsEditor id={g.id} canOperate={canOperate} />

                  {/* Resolved-members preview — evaluates the group (a dynamic
                      one runs its SQL) so the operator can see exactly who it
                      covers before wiring it into a schedule's target. */}
                  <button
                    type="button"
                    className="flex items-center gap-1 text-xs text-muted underline"
                    aria-expanded={open}
                    aria-controls={`preview-${g.id}`}
                    onClick={() =>
                      setPreview((prev) => {
                        const next = new Set(prev);
                        if (next.has(g.id)) next.delete(g.id);
                        else next.add(g.id);
                        return next;
                      })
                    }
                  >
                    {open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                    {open ? t('members.hide') : t('members.show')}
                  </button>
                  {open && <MembersPreview id={g.id} />}
                </CardContent>
              </Card>
            );
          })}
        </div>
      )}

      {editor && (
        <YamlEditorDialog
          open
          onOpenChange={(next) => {
            if (!next) setEditor(null);
          }}
          kind="group"
          mode={editor}
          // Open a Git-managed group read-only (parity with jobs/schedules/
          // views) so a ClickOps edit can't silently diverge from the repo.
          gitOrigin={
            editor.type === 'edit'
              ? (data?.find((g) => g.id === editor.id)?.origin ?? null)
              : null
          }
        />
      )}
    </div>
  );
}

// The group's notification addresses (`group_contacts` KV, via
// `GET`/`PUT /api/groups/{name}/email`). Fetched per card rather than from
// `GET /api/groups`: that endpoint walks every PC's membership row across two
// buckets to build its union — far more work than one KV get per group — and
// it omits a definition that currently resolves to nobody, which is exactly a
// group whose alert address you might still want to set.
function ContactsEditor({ id, canOperate }: { id: string; canOperate: boolean }) {
  const { t } = useTranslation('groups');
  const qc = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');

  const q = useQuery({
    queryKey: ['group-contacts', id],
    queryFn: () => apiFetch<GroupContacts>(`/api/groups/${encodeURIComponent(id)}/email`),
  });

  const save = useMutation({
    mutationFn: (emails: string[]) =>
      apiFetch<GroupContacts>(`/api/groups/${encodeURIComponent(id)}/email`, {
        method: 'PUT',
        body: JSON.stringify({ emails }),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['group-contacts', id] });
      toast.success(t('contacts.saved', { id }));
      setEditing(false);
    },
    onError: (e) => toast.error(formatError(e)),
  });

  const emails = q.data?.emails ?? [];

  if (editing) {
    return (
      <div className="flex items-center gap-1.5">
        <Mail className="size-3 shrink-0 text-muted" />
        <Input
          autoFocus
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={t('contacts.placeholder')}
          title={t('contacts.hint')}
          aria-label={t('contacts.edit', { id })}
          className="h-7 flex-1 text-xs"
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              // Guard against a double-submit from holding/mashing Enter
              // while the PUT is in flight.
              if (!save.isPending) save.mutate(parseEmails(draft));
            } else if (e.key === 'Escape') {
              setEditing(false);
            }
          }}
        />
        <Button
          size="sm"
          disabled={save.isPending}
          aria-label={t('contacts.save')}
          onClick={() => save.mutate(parseEmails(draft))}
        >
          <Check className="size-3" />
        </Button>
        <Button
          size="sm"
          variant="ghost"
          aria-label={t('contacts.cancel')}
          onClick={() => setEditing(false)}
        >
          <X className="size-3" />
        </Button>
      </div>
    );
  }

  return (
    <button
      type="button"
      // Also gated on the GET having succeeded. `emails` falls back to `[]` on
      // error, so an enabled trigger would open the editor on an empty draft
      // and the PUT — a whole-list replace — would wipe addresses the operator
      // never saw. The error text renders in place of the addresses below.
      disabled={!canOperate || q.isLoading || Boolean(q.error)}
      title={canOperate ? t('contacts.edit', { id }) : undefined}
      onClick={() => {
        setDraft(emails.join(', '));
        setEditing(true);
      }}
      className="flex items-center gap-1 text-left text-xs enabled:hover:text-fg disabled:cursor-default"
    >
      <Mail className="size-3 shrink-0 text-muted" />
      {q.isLoading ? (
        <span className="text-muted">{t('contacts.loading')}</span>
      ) : q.error ? (
        <span className="text-danger">{formatError(q.error)}</span>
      ) : emails.length === 0 ? (
        <span className="text-muted">{t('contacts.none')}</span>
      ) : (
        <span className="break-all">{emails.join(', ')}</span>
      )}
    </button>
  );
}

// Fetches + renders the resolved pc_id set for one group. A query error
// (e.g. a broken dynamic SQL) surfaces as a 400 with the reason, shown inline
// so the operator can debug the group without leaving the page.
function MembersPreview({ id }: { id: string }) {
  const { t } = useTranslation('groups');
  const q = useQuery({
    queryKey: ['group-defs', id, 'members'],
    queryFn: () =>
      apiFetch<GroupMembers>(`/api/group-defs/${encodeURIComponent(id)}/members`),
    staleTime: 30_000,
  });
  if (q.isLoading) {
    return (
      <div id={`preview-${id}`} className="text-muted text-xs">
        <Loader2 className="mr-1 inline size-3.5 animate-spin" />
        {t('members.loading')}
      </div>
    );
  }
  if (q.error) {
    return (
      <div
        id={`preview-${id}`}
        className="whitespace-pre-wrap rounded bg-danger/5 p-2 text-xs text-danger"
      >
        {formatError(q.error)}
      </div>
    );
  }
  const members = q.data?.members ?? [];
  return (
    <div id={`preview-${id}`} className="space-y-1 rounded border border-border/50 p-2">
      <div className="text-muted text-xs">{t('members.count', { count: members.length })}</div>
      {members.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {members.map((pc) => (
            <code key={pc} className="rounded bg-muted/10 px-1 text-[11px]">
              {pc}
            </code>
          ))}
        </div>
      )}
    </div>
  );
}
