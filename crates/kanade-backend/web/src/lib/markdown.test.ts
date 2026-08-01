import { describe, expect, it } from 'bun:test';
import { readFileSync } from 'node:fs';

/**
 * The notification body is authored and previewed in the SPA, then read
 * by the end user in the Client App's panel — two renderers, two
 * DOMPurify allowlists, one document. `lib/markdown.ts` says to keep
 * them in parity and `main.ts` says the same back, but nothing enforced
 * it, so the two could drift and the only symptom would be a preview
 * that lies: the operator sees a heading, the recipient gets flattened
 * text.
 *
 * Reading both lists out of the source keeps this honest without either
 * side having to export anything into the other's build (they are
 * separate bundles that never import each other).
 */
const SPA = 'src/lib/markdown.ts';
const CLIENT = '../../kanade-client/web/src/main.ts';

function tagsFrom(file: string, constName: string): string[] {
  const src = readFileSync(file, 'utf8');
  const start = src.indexOf(`const ${constName} = [`);
  if (start === -1) throw new Error(`${constName} not found in ${file}`);
  const body = src.slice(start, src.indexOf('];', start));
  // Quoted entries only — comment prose in the list must not be picked up.
  return [...body.matchAll(/["']([a-z][a-z0-9]*)["']/g)].map((m) => m[1]!);
}

describe('notification markdown allowlists', () => {
  it('are identical between the SPA and the Client App', () => {
    const spa = tagsFrom(SPA, 'ALLOWED_TAGS');
    const client = tagsFrom(CLIENT, 'NOTIF_MD_ALLOWED_TAGS');
    // Order is not the contract; membership is.
    expect([...client].sort()).toEqual([...spa].sort());
  });

  it('allow headings', () => {
    // The thing this PR added. A regression here is silent — DOMPurify
    // keeps the text and drops the tag — so assert it rather than
    // trusting the diff.
    const spa = tagsFrom(SPA, 'ALLOWED_TAGS');
    for (const h of ['h1', 'h2', 'h3', 'h4', 'h5', 'h6']) {
      expect(spa).toContain(h);
    }
  });

  it('still exclude the tags that carry scripting or remote loads', () => {
    // Guard the boundary the allowlist exists for: widening it for
    // headings must not have widened it for anything that executes or
    // phones out.
    const spa = tagsFrom(SPA, 'ALLOWED_TAGS');
    for (const bad of ['script', 'iframe', 'img', 'style', 'object', 'embed', 'form']) {
      expect(spa).not.toContain(bad);
    }
  });
});
