/**
 * Support codes — the operator side of the Client App's helpdesk unlock
 * (`client.unlock`, #1166).
 *
 * The types and the draft validator live here rather than in `Settings.tsx`
 * so they can be unit-tested: importing the page would pull in `@/i18n`,
 * whose `import.meta.glob` catalog loader only exists under Vite and throws
 * under `bun test`.
 *
 * Everything here mirrors a Rust source of truth. The backend re-validates
 * every field — it is the authority; this exists so the operator learns why a
 * draft is rejected without a round-trip.
 */

/** Mirrors `MAX_SUPPORT_UNLOCK_TTL_MINUTES` (kanade_shared::wire::server_settings):
 * 8 hours. An unlock grant reveals helpdesk-only jobs on an end user's
 * machine, so it must not outlive a working day even by operator error. */
export const MAX_SUPPORT_UNLOCK_TTL_MINUTES = 480;

/** Mirrors `MIN_SUPPORT_CODE_LEN` (the backend's support-code endpoint).
 * Short enough to stay typable over the phone, long enough that the agent's
 * per-machine rate limit (5 tries / 5 min) makes guessing hopeless. */
export const MIN_SUPPORT_CODE_LEN = 8;

/** Mirrors `DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES` (kanade_shared::wire::
 * server_settings): about the length of a helpdesk call. Shown as the TTL
 * field's placeholder and in the roster for a code that leaves it unset. */
export const DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES = 15;

/**
 * One operator-issued support code, as the API returns it. Mirrors
 * `kanade_shared::wire::SupportCode` MINUS the hash: `ServerSettings::redacted`
 * blanks it, so the secret never reaches the browser and an entry's mere
 * presence is what tells us the scope has a code set.
 */
export interface SupportCode {
  scope: string;
  // All three are `skip_serializing_if` on the Rust side, so an unset field
  // arrives as an ABSENT key rather than `null` — verified against a live
  // response: `{"scope":"admin"}` is what a code with no label / TTL / disable
  // flag looks like. Optional AND nullable so either shape decodes.
  label?: string | null;
  ttl_minutes?: number | null;
  disabled?: boolean;
}

/**
 * Body of `PUT /api/server-settings/support-codes/{scope}`. `code` is the
 * plaintext, sent once and never returned by anything.
 */
export interface SupportCodeBody {
  code: string;
  label: string | null;
  ttl_minutes: number | null;
  disabled: boolean;
}

/** A support-code draft as the form holds it — every field a string so each
 * can be blank. */
export interface SupportCodeDraft {
  scope: string;
  code: string;
  label: string;
  ttlMinutes: string;
}

/** Why a draft can't be submitted, or `null` when it can. Each variant maps
 * to a `server.supportCodes.errors.*` i18n key. */
export type SupportCodeError = 'scope' | 'codeShort' | 'codeWhitespace' | 'ttl' | 'label' | null;

/** Scope slug charset — mirrors `kanade_shared::manifest::is_valid_resource_id`.
 * The scope is compared byte-for-byte with a job's `client.unlock`, so
 * anything outside this can never match one. */
const SCOPE_RE = /^[A-Za-z0-9._-]+$/;

/**
 * Validate a support-code draft, returning the FIRST problem in field order
 * (the form shows one message, and the operator should be pointed at the first
 * field to fix rather than an arbitrary one).
 *
 * Two rules are easy to get subtly wrong, and both fail silently rather than
 * loudly if you do:
 *
 * - The **scope** may carry surrounding whitespace — the endpoint trims it
 *   before storing, so a padded slug still lands clean.
 * - The **code** may NOT. Whitespace inside a secret is significant, so the
 *   backend stores it verbatim; but the Client App trims what the user types,
 *   so a code with edge whitespace could be stored and then never redeemed.
 */
export function validateSupportCode(draft: SupportCodeDraft): SupportCodeError {
  if (!SCOPE_RE.test(draft.scope.trim())) return 'scope';
  if (draft.code !== draft.code.trim()) return 'codeWhitespace';
  // Count characters, not UTF-16 units: `.length` would accept four emoji as
  // "8 long" and the backend (which counts chars) would then reject it.
  if ([...draft.code].length < MIN_SUPPORT_CODE_LEN) return 'codeShort';
  if (draft.ttlMinutes.trim() !== '') {
    const n = Number(draft.ttlMinutes);
    if (!Number.isInteger(n) || n < 1 || n > MAX_SUPPORT_UNLOCK_TTL_MINUTES) return 'ttl';
  }
  // A blank label is fine (it's optional and falls back to the scope slug); a
  // whitespace-only one is a typo the backend rejects, so catch it here.
  if (draft.label !== '' && draft.label.trim() === '') return 'label';
  return null;
}
