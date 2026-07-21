/**
 * Monaco-backed YAML editor used by the Jobs / Schedules Add + Edit
 * modals. Wired to `monaco-yaml` so the operator gets schema-aware
 * completion + hover docs + inline validation against the JSON
 * Schemas served at `/api/schemas/manifest.json` and
 * `/api/schemas/schedule.json` (themselves derived from the live
 * Rust `Manifest` / `Schedule` types via `schemars`).
 *
 * **Bundle shape**: this module pulls in ~3 MB of `monaco-editor`
 * (~1 MB gzipped) plus ~200 KB of `monaco-yaml`. Consumers should
 * therefore wrap it in `React.lazy(() => import('@/components/YamlEditor'))`
 * so the Monaco chunk only downloads when the operator actually
 * opens an Add / Edit modal — the rest of the SPA keeps its ~100 KB
 * gzipped initial bundle.
 *
 * **Workers**: the editor + yaml language services run in
 * dedicated Web Workers via Vite's native `new Worker(new URL(...))`
 * support — no bundler plugin required. The worker chunks load on
 * editor mount and are cached for the SPA's lifetime.
 */

import { Editor, loader } from '@monaco-editor/react';
import * as monaco from 'monaco-editor';
import { useEffect, useState } from 'react';
// Vite-specific `?worker` imports — the bundler resolves these to
// hashed worker chunks under `dist/assets/` at build time and
// instantiating the class gives us a Worker pointing at that chunk.
// Anything else (raw `new Worker(new URL(...))` against a package
// path, dynamic import) trips Rollup's static-resolution pass and
// fails the build.
// `monaco-editor/editor/...`, not the pre-0.56 `monaco-editor/esm/vs/...`:
// 0.56 added an `exports` map (`"./*": "./esm/vs/*.js"`) that prepends the
// `esm/vs` segment itself, so the old deep path expands to `esm/vs/esm/vs/…`
// and no longer resolves. Same file, current spelling.
import EditorWorker from 'monaco-editor/editor/editor.worker?worker';
import { configureMonacoYaml } from 'monaco-yaml';
import YamlWorker from 'monaco-yaml/yaml.worker?worker';

// `MonacoEnvironment` is the global Monaco reads to learn how to
// spawn workers. Setting it once at module load is the documented
// pattern; Vite resolves the URL imports at build time to the right
// hashed chunks under `dist/assets/`.
declare global {
  interface Window {
    MonacoEnvironment?: {
      getWorker(workerId: string, label: string): Worker;
    };
  }
}

let monacoBootstrapped = false;
function ensureMonacoBootstrapped() {
  if (monacoBootstrapped) return;
  monacoBootstrapped = true;

  window.MonacoEnvironment = {
    getWorker(_workerId, label) {
      if (label === 'yaml') return new YamlWorker();
      return new EditorWorker();
    },
  };

  // `@monaco-editor/react` defaults to fetching Monaco off a CDN. We
  // host the SPA on an intranet box that may not have outbound
  // internet, so feed it our bundled copy instead — the chunk is
  // already in `dist/` thanks to the `monaco-editor` import above.
  loader.config({ monaco });

  // Wire monaco-yaml against the backend's schema endpoints. `path`
  // on the Editor element (`manifest.yaml` / `schedule.yaml`)
  // matches the `fileMatch` entries here, so the right schema gets
  // attached automatically — no per-instance config call needed.
  configureMonacoYaml(monaco, {
    validate: true,
    enableSchemaRequest: true,
    schemas: [
      {
        uri: '/api/schemas/manifest.json',
        fileMatch: ['manifest.yaml'],
      },
      {
        uri: '/api/schemas/schedule.json',
        fileMatch: ['schedule.yaml'],
      },
      {
        uri: '/api/schemas/view.json',
        fileMatch: ['view.yaml'],
      },
      {
        uri: '/api/schemas/group-def.json',
        fileMatch: ['group.yaml'],
      },
    ],
  });
}

export type YamlEditorKind = 'manifest' | 'schedule' | 'view' | 'group';

export interface YamlEditorProps {
  value: string;
  onChange: (next: string) => void;
  kind: YamlEditorKind;
  /** Optional override on the default `60vh` height. */
  height?: number | string;
  /** When true, the editor is read-only (used for diff / preview). */
  readOnly?: boolean;
}

// Wrap the `prefers-color-scheme: dark` MediaQueryList lookup so SSR
// shells, jsdom test environments, or any host that ships without
// `matchMedia` short-circuit to `null` instead of crashing on the
// `.matches` read.
function darkModeMql(): MediaQueryList | null {
  if (typeof window === 'undefined') return null;
  if (typeof window.matchMedia !== 'function') return null;
  return window.matchMedia('(prefers-color-scheme: dark)');
}

// Mirror the operator's OS `prefers-color-scheme` into a boolean and
// keep it in sync with system changes. Monaco's theme prop is the
// only viable knob — its CSS variables aren't reactive — so we feed
// it `vs-dark` / `vs` directly based on this hook.
function usePrefersDark(): boolean {
  const [dark, setDark] = useState(() => darkModeMql()?.matches ?? false);
  useEffect(() => {
    const mql = darkModeMql();
    if (!mql) return;
    const handler = (e: MediaQueryListEvent) => setDark(e.matches);
    mql.addEventListener('change', handler);
    return () => mql.removeEventListener('change', handler);
  }, []);
  return dark;
}

export default function YamlEditor({
  value,
  onChange,
  kind,
  height = '60vh',
  readOnly = false,
}: YamlEditorProps) {
  ensureMonacoBootstrapped();
  const path = `${kind}.yaml`;
  const prefersDark = usePrefersDark();

  return (
    <Editor
      height={height}
      language="yaml"
      path={path}
      value={value}
      onChange={(next) => onChange(next ?? '')}
      // Mirror the operator's OS preference so the editor doesn't
      // render as a dark island on a white card (or vice versa).
      // The two stock Monaco themes are the cheap path; if the SPA
      // ever ships an operator-level theme toggle, point this at
      // that source instead.
      theme={prefersDark ? 'vs-dark' : 'vs'}
      options={{
        minimap: { enabled: false },
        fontSize: 13,
        tabSize: 2,
        insertSpaces: true,
        wordWrap: 'on',
        scrollBeyondLastLine: false,
        readOnly,
        renderWhitespace: 'boundary',
        // Keep the operator's exact indentation: monaco's auto-indent
        // tries to be helpful inside a block scalar and would shift
        // pasted-in PowerShell. Off-by-default for our use case.
        autoIndent: 'keep',
      }}
    />
  );
}
