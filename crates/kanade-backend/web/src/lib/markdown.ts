import createDOMPurify from 'dompurify';
import { marked } from 'marked';

// Our own DOMPurify instance, NOT the shared default export. The link-rewriting
// hook below registers on `DOMPurify.addHook`, which is instance-global — using
// the default export would leak our `<a>` rewriting onto every other sanitize
// in the app (Monaco and friends). An isolated instance keeps the hook local.
const DOMPurify = createDOMPurify(window);

// Render an operator-authored notification body (Markdown) to a SANITIZED HTML
// subset for display in the SPA. Mirrors the Client App panel's renderer
// (crates/kanade-client/web/src/main.ts `renderMarkdown`) — same `marked` GFM
// options and the same allowed-tag subset, so what an operator previews here
// matches what endpoints render. The only intentional difference is link
// handling: the SPA is a normal browser, so links open in a new tab; the
// privileged Tauri WebView instead routes them through `open_external_url`.
//
// We never inject raw HTML: `marked` produces the HTML and `DOMPurify` strips
// everything outside the allowlist (no <img>, <script>, event handlers, or
// `javascript:` URLs).
//
// Options are passed per-call (below) rather than via `marked.setOptions`, so
// we don't mutate the shared `marked` singleton's global state. `async: false`
// picks marked's synchronous overload, which returns `string`.
const MARKED_OPTS = { async: false, gfm: true, breaks: true } as const;

// Keep this list in parity with the Client App renderer's allowlist.
const ALLOWED_TAGS = [
  'p', 'br', 'strong', 'em', 'del', 'code', 'pre', 'blockquote',
  'ul', 'ol', 'li', 'a',
  'table', 'thead', 'tbody', 'tr', 'th', 'td',
  // Headings. A fleet-wide notice is a document — steps, then how to
  // report, then who to contact — and without these an operator's `##`
  // was silently flattened to body text by DOMPurify's KEEP_CONTENT,
  // losing the structure while looking like it had been accepted. The
  // CSS caps them well below browser defaults so an `h1` can't tower
  // over the card title above it.
  'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
];
// `target` / `rel` are added by the link hook below; `align` is what GFM emits
// for table-column alignment.
const ALLOWED_ATTR = ['href', 'target', 'rel', 'align'];

// Open http(s) links in a new tab with `noopener`; neutralise any other scheme
// (DOMPurify already drops `javascript:` etc. — this also makes `mailto:` and
// relative links inert, matching the client's http(s)-only policy).
DOMPurify.addHook('afterSanitizeAttributes', (node) => {
  if (node.nodeName !== 'A') return;
  const href = node.getAttribute('href') ?? '';
  if (/^https?:\/\//i.test(href)) {
    node.setAttribute('target', '_blank');
    node.setAttribute('rel', 'noopener noreferrer');
  } else {
    node.removeAttribute('href');
  }
});

// Cache by body so the detail view and the live compose/edit preview don't
// re-parse+sanitize unchanged input. Bounded: the preview re-renders on every
// keystroke (each a distinct body), so cap the size and clear wholesale when
// full rather than letting it grow without limit.
const RENDER_CACHE = new Map<string, string>();
const RENDER_CACHE_MAX = 256;

export function renderNotificationMarkdown(body: string): string {
  const cached = RENDER_CACHE.get(body);
  if (cached !== undefined) return cached;
  const html = marked.parse(body, MARKED_OPTS);
  const safe = DOMPurify.sanitize(html, { ALLOWED_TAGS, ALLOWED_ATTR });
  if (RENDER_CACHE.size >= RENDER_CACHE_MAX) RENDER_CACHE.clear();
  RENDER_CACHE.set(body, safe);
  return safe;
}
