/**
 * Issue #1342: fold the Events page's filter vocabularies into groups.
 *
 * The `kind` / `source` chip lists are populated from the backend's
 * `DISTINCT` queries, which is what lets a new collector's vocabulary
 * appear without an SPA change (#391). The flip side is that the lists
 * grow without bound — 23 kinds + 12 sources at the time of writing,
 * all rendered flat above the results.
 *
 * Both vocabularies already carry structure the flat list discards, so
 * grouping is a matter of reading it back out rather than maintaining a
 * catalogue by hand. Three tiers, tried in order:
 *
 *   1. an explicit category table, for values whose kinship is semantic
 *      rather than lexical (`logon` / `lock` / `boot` share no prefix);
 *   2. prefix derivation, which picks up whole families for free —
 *      `command_signature_*`, `agent_*`, `winlog:*` — including ones
 *      that do not exist yet;
 *   3. an "other" bucket that catches everything else.
 *
 * Tier 3 is the load-bearing one. It is what makes this safe to ship
 * against a vocabulary nobody controls: a newly-emitted value can be
 * OVERLOOKED (it sits in "other" rather than somewhere meaningful), but
 * it can never be DROPPED, and no input can leave the UI in a state
 * where a chip the operator was filtering by has silently vanished.
 * `everyValueIsGroupedExactlyOnce` in the tests pins that down.
 */

/** One folded group of chip values. */
export type VocabGroup = {
  /**
   * Unique across the returned groups — the React key and the fold-state
   * handle.
   *
   * Deliberately NOT `id`. The tiers draw their ids from independent
   * namespaces that can collide: a source `other:foo` derives the prefix
   * id `other`, which is also the catch-all's id, and a future
   * `session_expired` / `session_renewed` family would derive `session`
   * alongside the existing `session` category. Either collision gives two
   * groups the same React key, so they expand together and the reconciler
   * warns. Prefixing by tier separates identity from label, and is
   * provably unique: tier heads cannot contain the separator, so no two
   * derived ids collide either.
   */
  key: string;
  /**
   * The label source: a category id to translate (`session`), the literal
   * prefix for a derived group (`winlog`), or the catch-all's id.
   * Not unique on its own — see `key`.
   */
  id: string;
  /**
   * How the caller should label the group. `category` and `other` resolve
   * through i18n; `prefix` is a literal drawn from the data and therefore
   * has no translation — a new collector's namespace has to render as
   * itself.
   */
  labelKind: 'category' | 'prefix' | 'other';
  /**
   * The separator that produced `id`, for `prefix` groups. Callers that
   * strip the shared prefix off a member's label need it: assuming one
   * character happens to hold for `_` and `:` and would silently mangle
   * labels the day a vocabulary arrives with a longer delimiter.
   */
  separator: string;
  /** The vocabulary values in this group, in the order they were given. */
  values: string[];
};

/**
 * A member's label with the group's shared prefix removed — `winlog:Security`
 * reads as `Security` under a `winlog` header. Only `prefix` groups have
 * anything redundant to strip; category members share no prefix, and
 * returning the value unchanged for them keeps callers branch-free.
 */
export function shortLabel(group: VocabGroup, value: string): string {
  if (group.labelKind !== 'prefix') return value;
  const head = group.id + group.separator;
  return value.startsWith(head) ? value.slice(head.length) : value;
}

/** An explicit semantic grouping, for values that share no useful prefix. */
export type VocabCategory = { id: string; values: readonly string[] };

export type GroupOptions = {
  /** Consulted first. Values not listed here fall through to derivation. */
  categories?: readonly VocabCategory[];
  /** The character that delimits a value's namespace from its leaf. */
  separator: string;
  /**
   * How many values must share a prefix before it becomes a group.
   *
   * This is not a tuning knob — it encodes what the separator MEANS in
   * each vocabulary. In `source`, `:` is an explicit namespace marker
   * (`winlog:Security`), so a prefix is real even with one member and
   * this is 1. In `kind`, `_` is ordinary word separation, so a prefix
   * is only evidence of a family when several values share it; at 1 we
   * would invent an "unexpected" group holding `unexpected_shutdown`
   * alone. Hence 2 there.
   */
  minGroupSize: number;
};

/** The catch-all group's id. Exported so callers can special-case it. */
export const OTHER_GROUP_ID = 'other';

/**
 * The longest `separator`-delimited prefix shared by every value, or null
 * when they only agree on the first segment they were bucketed by.
 *
 * Bucketing by first segment alone would label the four
 * `command_signature_*` kinds as "command", which reads as a group about
 * commands rather than about signature verification. Extending to the
 * longest COMMON prefix recovers the real family name from the data, so
 * the label stays honest without a hand-written entry.
 *
 * Never returns the whole of any value: a prefix equal to a member would
 * make that member's own chip label empty.
 */
function longestSharedPrefix(values: string[], separator: string): string | null {
  if (values.length === 0) return null;
  const split = values.map((v) => v.split(separator));
  // The shortest value bounds how far the prefix can run, and it must
  // keep at least one trailing segment of its own.
  const maxLen = Math.min(...split.map((s) => s.length)) - 1;
  let shared = 0;
  for (let i = 0; i < maxLen; i++) {
    if (split.every((s) => s[i] === split[0][i])) shared = i + 1;
    else break;
  }
  return shared > 0 ? split[0].slice(0, shared).join(separator) : null;
}

/**
 * Fold a flat vocabulary into groups.
 *
 * Input order is preserved within each group, and groups come out in a
 * deterministic order — explicit categories in table order, then derived
 * prefixes in first-appearance order, then "other" last. Stability
 * matters here beyond tidiness: these drive fold state keyed by group id,
 * so an order that shifted as the vocabulary grew would move the chip an
 * operator was reaching for.
 */
export function groupVocabulary(
  values: readonly string[],
  { categories = [], separator, minGroupSize }: GroupOptions,
): VocabGroup[] {
  // Deduped defensively: the backend's DISTINCT already guarantees it,
  // but a repeated value would otherwise be rendered twice and break the
  // exactly-once invariant the tests rely on.
  const remaining = Array.from(new Set(values));
  const groups: VocabGroup[] = [];

  // Tier 1 — explicit categories, in table order.
  for (const cat of categories) {
    const matched = remaining.filter((v) => cat.values.includes(v));
    if (matched.length > 0) {
      groups.push({
        key: `category:${cat.id}`,
        id: cat.id,
        labelKind: 'category',
        separator,
        values: matched,
      });
    }
  }
  const claimed = new Set(groups.flatMap((g) => g.values));
  const leftover = remaining.filter((v) => !claimed.has(v));

  // Tier 2 — derive from the separator. Bucket by first segment, then
  // let each bucket name itself with the longest prefix its members
  // actually share.
  const buckets = new Map<string, string[]>();
  const unprefixed: string[] = [];
  for (const v of leftover) {
    const idx = v.indexOf(separator);
    // No separator, or one with nothing after it, means no namespace to
    // read — straight to "other".
    if (idx <= 0 || idx === v.length - separator.length) {
      unprefixed.push(v);
      continue;
    }
    const head = v.slice(0, idx);
    const bucket = buckets.get(head);
    if (bucket) bucket.push(v);
    else buckets.set(head, [v]);
  }

  const other: string[] = [...unprefixed];
  for (const [head, members] of buckets) {
    if (members.length < minGroupSize) {
      other.push(...members);
      continue;
    }
    // A single member cannot "share" a longer prefix with anyone, so it
    // keeps the bucket head it was filed under.
    const id =
      members.length > 1 ? (longestSharedPrefix(members, separator) ?? head) : head;
    groups.push({ key: `prefix:${id}`, id, labelKind: 'prefix', separator, values: members });
  }

  // Tier 3 — the catch-all, always last, and omitted when empty so the
  // UI does not render an empty fold.
  if (other.length > 0) {
    // Restored to input order: `other` is assembled from two passes
    // (unprefixed first, then undersized buckets), which is not the
    // order the operator saw in the backend's sorted list.
    const order = new Map(remaining.map((v, i) => [v, i]));
    other.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0));
    groups.push({
      key: OTHER_GROUP_ID,
      id: OTHER_GROUP_ID,
      labelKind: 'other',
      separator,
      values: other,
    });
  }

  return groups;
}

/**
 * The `kind` categories that prefix derivation cannot find, because the
 * kinship is semantic rather than lexical. Everything with a shared
 * prefix (`agent_*`, `command_signature_*`, `log_service_*`) is
 * deliberately absent — derivation handles those, and listing them here
 * would freeze the families at today's membership.
 */
export const KIND_CATEGORIES: readonly VocabCategory[] = [
  { id: 'session', values: ['logon', 'logoff', 'lock', 'unlock'] },
  { id: 'power', values: ['boot', 'shutdown', 'sleep', 'resume', 'unexpected_shutdown'] },
  { id: 'presence', values: ['active', 'idle', 'presence'] },
  { id: 'activity', values: ['web_visit', 'app_sample'] },
];

/** Fold the `kind` vocabulary. See `minGroupSize` on why `_` needs 2. */
export function groupKinds(values: readonly string[]): VocabGroup[] {
  return groupVocabulary(values, {
    categories: KIND_CATEGORIES,
    separator: '_',
    minGroupSize: 2,
  });
}

/** Fold the `source` vocabulary — pure derivation, no table. */
export function groupSources(values: readonly string[]): VocabGroup[] {
  return groupVocabulary(values, { separator: ':', minGroupSize: 1 });
}
