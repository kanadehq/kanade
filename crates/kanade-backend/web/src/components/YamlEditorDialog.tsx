/**
 * Add / Edit modal wrapper around the lazy-loaded `YamlEditor`. One
 * component covers both Jobs and Schedules; the `kind` prop picks the
 * matching endpoint (`/api/jobs` vs `/api/schedules`) and the schema
 * the editor validates against.
 *
 * Create mode opens with a minimal template so the operator has a
 * head start on the required fields; edit mode fetches the operator's
 * own YAML source via `GET /api/{kind}/{id}/yaml` so comments and
 * script block-scalar indentation come back exactly as written.
 *
 * Save flow: POST the raw editor text with
 * `Content-Type: application/yaml`. The backend mirrors the bytes
 * verbatim into the parallel `BUCKET_*_YAML` store so subsequent
 * edits stay round-trip clean. Validation errors come back as
 * HTTP 400 with the parser error in the body — surfaced inline near
 * the Save button.
 */

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Save } from 'lucide-react';
import { lazy, Suspense, useEffect, useRef, useState } from 'react';

import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { apiFetch, apiFetchText, ApiError } from '@/lib/api';

const YamlEditor = lazy(() => import('./YamlEditor'));

export type EditorKind = 'manifest' | 'schedule';
export type EditorMode = { type: 'create' } | { type: 'edit'; id: string };

interface YamlEditorDialogProps {
  open: boolean;
  onOpenChange: (next: boolean) => void;
  kind: EditorKind;
  mode: EditorMode;
}

const JOB_TEMPLATE = `# A new job manifest. id + version + execute are required.
id: my-job
version: "1.0.0"
description: ""
execute:
  shell: powershell
  script: |
    Write-Output "hello from kanade"
  timeout: 30s
`;

const SCHEDULE_TEMPLATE = `# A new cron schedule. id + cron + job_id are required.
id: my-schedule
cron: "0 0 * * * *"  # every hour on the minute
job_id: my-job
enabled: true
`;

function templateFor(kind: EditorKind): string {
  return kind === 'manifest' ? JOB_TEMPLATE : SCHEDULE_TEMPLATE;
}

function endpointBase(kind: EditorKind): string {
  return kind === 'manifest' ? '/api/jobs' : '/api/schedules';
}

function listQueryKey(kind: EditorKind): readonly string[] {
  return kind === 'manifest' ? ['jobs'] : ['schedules'];
}

async function fetchYaml(kind: EditorKind, id: string): Promise<string> {
  return apiFetchText(`${endpointBase(kind)}/${encodeURIComponent(id)}/yaml`);
}

async function postYaml(kind: EditorKind, yaml: string): Promise<unknown> {
  return apiFetch(endpointBase(kind), {
    method: 'POST',
    headers: { 'Content-Type': 'application/yaml' },
    body: yaml,
  });
}

function formatError(err: unknown): string {
  return err instanceof ApiError
    ? `${err.status} — ${err.body || err.message}`
    : String(err);
}

export function YamlEditorDialog({ open, onOpenChange, kind, mode }: YamlEditorDialogProps) {
  const qc = useQueryClient();
  const [text, setText] = useState<string>(() =>
    mode.type === 'create' ? templateFor(kind) : '',
  );

  // For edit mode, pull the saved YAML mirror via GET …/yaml so the
  // editor opens on the operator's exact source (comments + block
  // scalar indent preserved). The query is keyed by mode id; switching
  // to a different row in the same dialog instance refetches.
  // `refetchOnWindowFocus: false` + `staleTime: Infinity` keep
  // background refreshes from clobbering the operator's in-progress
  // edits when they alt-tab back to the dialog (gemini PR-B
  // feedback).
  const editId = mode.type === 'edit' ? mode.id : null;
  const fetched = useQuery({
    queryKey: [kind, 'yaml', editId],
    enabled: open && editId !== null,
    queryFn: () => fetchYaml(kind, editId!),
    refetchOnWindowFocus: false,
    staleTime: Infinity,
  });

  // Seed the editor exactly once per (open, session) pair. We
  // track "have we initialised the textarea for this session yet?"
  // via a ref so a later same-session re-render (e.g. mutation
  // settled, query.refetch()) can't trample the operator's edits.
  // sessionKey changes when the dialog opens against a different
  // row, which re-arms initialisation.
  const initialisedFor = useRef<string | null>(null);
  useEffect(() => {
    if (!open) {
      initialisedFor.current = null;
      return;
    }
    const sessionKey = mode.type === 'create' ? '__create__' : `edit:${mode.id}`;
    if (initialisedFor.current === sessionKey) return;
    if (mode.type === 'create') {
      setText(templateFor(kind));
      initialisedFor.current = sessionKey;
    } else if (fetched.data !== undefined) {
      setText(fetched.data);
      initialisedFor.current = sessionKey;
    }
  }, [open, mode, kind, fetched.data]);

  const save = useMutation({
    mutationFn: () => postYaml(kind, text),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: listQueryKey(kind) });
      onOpenChange(false);
    },
  });

  const title =
    mode.type === 'create'
      ? kind === 'manifest'
        ? 'New job'
        : 'New schedule'
      : kind === 'manifest'
        ? `Edit job: ${mode.id}`
        : `Edit schedule: ${mode.id}`;

  const isLoadingExisting = mode.type === 'edit' && fetched.isLoading;
  const hasLoadError = mode.type === 'edit' && Boolean(fetched.error);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-5xl">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            Schema-aware editor — comments and indentation round-trip through{' '}
            <code className="text-xs">application/yaml</code>. Hover field names for docs.
          </DialogDescription>
        </DialogHeader>

        {isLoadingExisting ? (
          <div className="flex items-center gap-2 text-muted h-[60vh] justify-center">
            <Loader2 className="size-4 animate-spin" />
            loading current YAML…
          </div>
        ) : fetched.error ? (
          <div className="text-danger text-sm whitespace-pre-wrap bg-danger/5 p-2 rounded">
            Couldn't load YAML: {formatError(fetched.error)}
          </div>
        ) : (
          <Suspense
            fallback={
              <div className="flex items-center gap-2 text-muted h-[60vh] justify-center">
                <Loader2 className="size-4 animate-spin" />
                loading editor…
              </div>
            }
          >
            <YamlEditor value={text} onChange={setText} kind={kind} />
          </Suspense>
        )}

        {save.error && (
          <div className="text-danger text-xs whitespace-pre-wrap bg-danger/5 p-2 rounded">
            {formatError(save.error)}
          </div>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={save.isPending}>
            Cancel
          </Button>
          <Button
            variant="default"
            onClick={() => save.mutate()}
            disabled={save.isPending || isLoadingExisting || hasLoadError}
          >
            {save.isPending ? (
              <>
                <Loader2 className="size-3.5 animate-spin" />
                saving…
              </>
            ) : (
              <>
                <Save className="size-3.5" />
                Save
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
