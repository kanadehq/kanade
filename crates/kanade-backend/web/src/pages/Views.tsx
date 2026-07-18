import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Pencil, Plus, Trash2 } from 'lucide-react';
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

// Mirrors `kanade_shared::manifest::View` (the bits the list renders).
// `ViewWidget` is a DISPLAY-ONLY subset of the real `AggregateWidget` —
// the page only needs the dashboard label to chip it; the full shape
// (kind / agg / group_by / order / …) lives in the YAML editor.
type ViewWidget = { dashboard: string; title: string; render: string; scope?: string };
type ViewRow = {
  id: string;
  description?: string | null;
  /** Optional — the backend omits an EMPTY list (`skip_serializing_if =
   *  "Vec::is_empty"`), so a SQL-only view carries no `widgets` key at all.
   *  Must be treated as possibly-undefined or the list crashes on it (#901
   *  views like kev-exposure / eol-exposure are sql_widgets-only). */
  widgets?: ViewWidget[];
  /** SQL-backed widgets (#901). Same omit-when-empty rule; the list only
   *  needs the count, so the element shape is left minimal. */
  sql_widgets?: unknown[];
  tags?: string[];
  /** GitOps provenance (#678) — present ⇒ open the editor read-only. */
  origin?: RepoOrigin | null;
};

export function Views() {
  const { t } = useTranslation('views');
  const qc = useQueryClient();
  const confirm = useConfirm();
  const [editor, setEditor] = useState<EditorMode | null>(null);

  const { data, error, isLoading } = useQuery({
    queryKey: ['views'],
    queryFn: () => apiFetch<ViewRow[]>('/api/views'),
  });

  const del = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/views/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: (_r, id) => {
      qc.invalidateQueries({ queryKey: ['views'] });
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
        <Button onClick={() => setEditor({ type: 'create' })}>
          <Plus className="size-4" />
          {t('actions.new')}
        </Button>
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
          {data.map((v) => {
            // `widgets` / `sql_widgets` are omitted when empty (see the type),
            // so default them before mapping/counting — a SQL-only view has no
            // `widgets` key and would otherwise crash the whole page.
            const widgets = v.widgets ?? [];
            const widgetCount = widgets.length + (v.sql_widgets?.length ?? 0);
            const dashboards = [...new Set(widgets.map((w) => w.dashboard))];
            return (
              <Card key={v.id}>
                <CardHeader className="flex flex-row items-start justify-between gap-2">
                  <div className="min-w-0">
                    <CardTitle className="text-sm">
                      <code>{v.id}</code>
                    </CardTitle>
                    {v.description && (
                      <p className="mt-1 text-muted text-xs">{v.description}</p>
                    )}
                  </div>
                  <div className="flex shrink-0 gap-1">
                    <Button
                      variant="secondary"
                      size="sm"
                      onClick={() => setEditor({ type: 'edit', id: v.id })}
                      aria-label={t('actions.editAria', { id: v.id })}
                    >
                      <Pencil className="size-3.5" />
                    </Button>
                    <Button
                      variant="secondary"
                      size="sm"
                      aria-label={t('actions.deleteAria', { id: v.id })}
                      onClick={async () => {
                        const ok = await confirm({
                          title: t('confirm.deleteTitle', { id: v.id }),
                          description: t('confirm.deleteDescription'),
                          confirmLabel: t('confirm.deleteLabel'),
                        });
                        if (ok) del.mutate(v.id);
                      }}
                    >
                      <Trash2 className="size-3.5 text-danger" />
                    </Button>
                  </div>
                </CardHeader>
                <CardContent className="space-y-2">
                  <div className="text-muted text-xs">
                    {t('widgetCount', { count: widgetCount })}
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {dashboards.map((d) => (
                      <Badge key={d} variant="violet">
                        {d}
                      </Badge>
                    ))}
                  </div>
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
          kind="view"
          mode={editor}
          // Open a Git-managed view read-only (parity with jobs/schedules)
          // so a ClickOps edit can't silently diverge from the repo.
          gitOrigin={
            editor.type === 'edit'
              ? (data?.find((v) => v.id === editor.id)?.origin ?? null)
              : null
          }
        />
      )}
    </div>
  );
}
