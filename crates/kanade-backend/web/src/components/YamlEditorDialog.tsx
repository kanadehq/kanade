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
import { ExternalLink, GitBranch, Loader2, Save } from 'lucide-react';
import { lazy, Suspense, useEffect, useRef, useState } from 'react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { apiFetch, apiFetchText, formatError } from '@/lib/api';

const YamlEditor = lazy(() => import('./YamlEditor'));

export type EditorKind = 'manifest' | 'schedule' | 'view' | 'group';
export type EditorMode = { type: 'create' } | { type: 'edit'; id: string };

/** GitOps provenance (#678/#695) — mirrors `kanade_shared::manifest::RepoOrigin`. */
export type RepoOrigin = {
  /** Repo-relative path of the source YAML (forward slashes). */
  path: string;
  /** `origin` remote URL, when the repo has one. */
  repo?: string | null;
  /** Repo-relative path of a job's inlined `script_file:`, when any
   *  (always absent for schedules). */
  script_file?: string | null;
};

interface YamlEditorDialogProps {
  open: boolean;
  onOpenChange: (next: boolean) => void;
  kind: EditorKind;
  mode: EditorMode;
  /** When set on an edit, the entry is Git-managed (SPEC §3 GitOps): the
   *  editor opens read-only and points the operator back at the repo
   *  instead of letting a SPA edit silently diverge from Git
   *  (#678 jobs / #695 schedules). */
  gitOrigin?: RepoOrigin | null;
}

/** Best-effort conversion of a Git remote URL to an https repo link.
 *  `git@host:owner/repo(.git)` → `https://host/owner/repo`; an http(s)
 *  remote just loses a trailing `.git`. Returns null when it can't make
 *  a confident web URL (we only link the repo root, not a blob path —
 *  we don't know the branch). */
function repoWebUrl(remote?: string | null): string | null {
  if (!remote) return null;
  const r = remote.trim();
  const ssh = r.match(/^git@([^:]+):(.+?)(?:\.git)?$/);
  if (ssh) return `https://${ssh[1]}/${ssh[2]}`;
  const stripped = r.replace(/\.git$/, '');
  if (/^https?:\/\//.test(stripped)) {
    // Defence-in-depth: strip any embedded userinfo (`token@host`) so a
    // credential-bearing remote never becomes a clickable link. The CLI
    // already redacts on capture, but a hand-written origin could slip
    // one through (#679 review).
    return stripped.replace(/^(https?:\/\/)[^/@]*@/, '$1');
  }
  return null;
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

const SCHEDULE_TEMPLATE = `# A new schedule. id + when + job_id are required.
# when shapes (#418):
#   per_pc: once                         — run once on every pc, forever catching new ones
#   per_pc: { every: 6h }                — re-run per pc after the interval
#   per_target: { every: 24h }           — one delegate pc per interval (backend only)
#   calendar: { at: "09:00", days: [mon-fri] }  — fire at a wall-clock time (tz below)
#   calendar: { at: "2026-06-10 09:00" }        — one-shot: fire once on that date
id: my-schedule
when:
  per_pc: { every: 1h }
job_id: my-job
target:
  all: true
# tz: local   # local (default) | utc — applies to calendar at + active + window
# constraints:
#   window: "22:00-05:00"   # only fire within this wall-clock window (in tz)
enabled: true
`;

const VIEW_TEMPLATE = `# A new Analytics view (#743). id + widgets are required.
# A view declares dashboards over obs_events with no execute / schedule.
id: my-view
description: ""
widgets:
  - dashboard: Reliability
    title: Unexpected shutdowns by PC
    scope: fleet            # pc | fleet
    kind: unexpected_shutdown
    agg: count              # count | ratio | sum
    group_by: pc_id         # JSON path, or the literal pc_id (fleet ranking)
    render: bar             # bar | gauge | timeline | stat
    # order: 0              # optional sort weight (lower = earlier)
`;

const GROUP_TEMPLATE = `# A new declared group (#1032). id + exactly one of members / query.
# Static — a literal, git-reviewable membership list:
id: my-group
members:
  - PC-A1
  - LAB-03
# Dynamic — membership derived from a read-only SQL query returning a
# \`pc_id\` column (uncomment, and drop \`members:\` above):
# query: |
#   SELECT pc_id FROM inventory_facts
#   WHERE job_id = 'inventory-hw'
#     AND json_extract(facts_json, '$.os_name') NOT LIKE '%Server%'
# refresh: 30m   # dynamic-group recompute cadence (default 10m)
`;

function templateFor(kind: EditorKind): string {
  switch (kind) {
    case 'manifest':
      return JOB_TEMPLATE;
    case 'schedule':
      return SCHEDULE_TEMPLATE;
    case 'view':
      return VIEW_TEMPLATE;
    case 'group':
      return GROUP_TEMPLATE;
  }
}

function endpointBase(kind: EditorKind): string {
  switch (kind) {
    case 'manifest':
      return '/api/jobs';
    case 'schedule':
      return '/api/schedules';
    case 'view':
      return '/api/views';
    case 'group':
      return '/api/group-defs';
  }
}

function listQueryKey(kind: EditorKind): readonly string[] {
  return [endpointBase(kind).replace('/api/', '')];
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

export function YamlEditorDialog({
  open,
  onOpenChange,
  kind,
  mode,
  gitOrigin,
}: YamlEditorDialogProps) {
  const qc = useQueryClient();
  // #678: a Git-managed job is read-only in the SPA — editing belongs in
  // the repo (GitOps). Only meaningful on an edit; create is always
  // SPA-born and editable.
  const gitManaged = mode.type === 'edit' && Boolean(gitOrigin);
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
      toast.success(mode.type === 'create' ? `Created ${kind}` : `Saved ${kind}: ${mode.id}`);
    },
    onError: (e) => toast.error(`Save failed: ${formatError(e)}`),
  });

  // Display noun per kind ('manifest' surfaces to operators as "job").
  const noun = kind === 'manifest' ? 'job' : kind; // job | schedule | view | group
  const Noun = noun.charAt(0).toUpperCase() + noun.slice(1);
  const title =
    mode.type === 'create'
      ? `New ${noun}`
      : gitManaged
        ? `${Noun}: ${mode.id} · read-only`
        : `Edit ${noun}: ${mode.id}`;

  const repoUrl = repoWebUrl(gitOrigin?.repo);
  // #695: the read-only banner is shared across kinds, so the apply
  // command and the source-path label follow `kind`.
  // The CLI verb differs for groups: `kanade group def create` (the plain
  // `kanade group …` namespace is the imperative membership commands).
  const applyCmd = kind === 'group' ? 'kanade group def create' : `kanade ${noun} create`;
  // The YAML's source-path label is the manifest's top-level key name.
  const sourceLabel = `${kind === 'manifest' ? 'manifest' : noun}:`;

  const isLoadingExisting = mode.type === 'edit' && fetched.isLoading;
  const hasLoadError = mode.type === 'edit' && Boolean(fetched.error);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-5xl">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            {gitManaged ? (
              <>
                Managed in Git — read-only. Edit the source in the repo, then run{' '}
                <code className="text-xs">{applyCmd}</code> to apply (SPEC §3 GitOps).
              </>
            ) : (
              <>
                Schema-aware editor — comments and indentation round-trip through{' '}
                <code className="text-xs">application/yaml</code>. Hover field names for docs.
              </>
            )}
          </DialogDescription>
        </DialogHeader>

        {gitManaged && gitOrigin && (
          // #678: point the operator at the Git source of truth. The repo
          // link (when a remote is configured) targets the repository
          // root — we can't safely build a blob URL without the branch.
          <div className="rounded border border-violet/30 bg-violet/5 p-2 text-xs">
            <div className="flex items-center gap-1.5 font-medium text-violet">
              <GitBranch className="size-3.5" />
              Managed in Git
            </div>
            <div className="mt-1 flex flex-wrap items-center gap-1.5 text-muted">
              <span>{sourceLabel}</span>
              <code className="break-all text-fg">{gitOrigin.path}</code>
              {repoUrl && (
                <a
                  href={repoUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-0.5 text-violet hover:underline"
                >
                  open repo
                  <ExternalLink className="size-3" />
                </a>
              )}
            </div>
            {gitOrigin.script_file && (
              <div className="mt-0.5 flex flex-wrap items-center gap-1.5 text-muted">
                <span>↳ script:</span>
                <code className="break-all text-fg">{gitOrigin.script_file}</code>
              </div>
            )}
          </div>
        )}

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
            <YamlEditor value={text} onChange={setText} kind={kind} readOnly={gitManaged} />
          </Suspense>
        )}

        {save.error && (
          <div className="text-danger text-xs whitespace-pre-wrap bg-danger/5 p-2 rounded">
            {formatError(save.error)}
          </div>
        )}

        <DialogFooter>
          {gitManaged ? (
            // Read-only: no Save. Editing happens in Git, so the only
            // action is to dismiss the viewer.
            <Button variant="default" onClick={() => onOpenChange(false)}>
              Close
            </Button>
          ) : (
            <>
              <Button
                variant="ghost"
                onClick={() => onOpenChange(false)}
                disabled={save.isPending}
              >
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
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
