import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ChevronDown, ChevronRight, Loader2, Pencil, Plus, Trash2 } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { type EditorMode, type RepoOrigin, YamlEditorDialog } from '@/components/YamlEditorDialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
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

function isDynamic(g: GroupDefRow): boolean {
  return typeof g.query === 'string' && g.query.trim().length > 0;
}

export function GroupDefs() {
  const { t } = useTranslation('groupdefs');
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
    mutationFn: (id: string) =>
      apiFetch(`/api/group-defs/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: (_r, id) => {
      qc.invalidateQueries({ queryKey: ['group-defs'] });
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

// Fetches + renders the resolved pc_id set for one group. A query error
// (e.g. a broken dynamic SQL) surfaces as a 400 with the reason, shown inline
// so the operator can debug the group without leaving the page.
function MembersPreview({ id }: { id: string }) {
  const { t } = useTranslation('groupdefs');
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
