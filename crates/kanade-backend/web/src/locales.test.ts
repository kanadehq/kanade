import { describe, expect, test } from 'bun:test';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

/**
 * Every namespace must carry the same keys in both locales.
 *
 * A missing key does not fail loudly: i18next falls back to `fallbackLng`,
 * so the Japanese console quietly renders that one string in English and
 * nothing anywhere reports it. That is exactly how the YAML editor dialog
 * (#1293) shipped an entirely English modal inside a Japanese page — the
 * component simply never called `useTranslation`, and no test could see it.
 *
 * This guards the neighbouring failure: a namespace that exists in both
 * locales but has drifted apart, which is what happens when someone adds a
 * string to `en` and forgets `ja`.
 */

const DIR = join(import.meta.dir, 'locales');
const LOCALES = ['en', 'ja'] as const;

/** CLDR plural categories i18next appends to a key. */
const PLURAL_SUFFIX = /_(zero|one|two|few|many|other)$/;

/**
 * Flatten to dotted paths, dropping the plural suffix.
 *
 * Comparing raw keys would be wrong, not stricter: English needs `_one` and
 * `_other`, Japanese has a single plural category and needs only `_other`.
 * Demanding an exact match would push a meaningless `_one` into every
 * Japanese catalogue.
 */
function baseKeys(value: unknown, prefix = ''): Set<string> {
  const out = new Set<string>();
  for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
    const path = prefix ? `${prefix}.${k}` : k;
    if (v !== null && typeof v === 'object' && !Array.isArray(v)) {
      for (const nested of baseKeys(v, path)) out.add(nested);
    } else {
      out.add(path.replace(PLURAL_SUFFIX, ''));
    }
  }
  return out;
}

const read = (locale: string, ns: string) =>
  JSON.parse(readFileSync(join(DIR, locale, ns), 'utf-8')) as unknown;

const namespaces = Object.fromEntries(
  LOCALES.map((l) => [l, readdirSync(join(DIR, l)).filter((f) => f.endsWith('.json')).sort()]),
) as Record<(typeof LOCALES)[number], string[]>;

describe('locale catalogues', () => {
  test('both locales declare the same namespaces', () => {
    expect(namespaces.ja).toEqual(namespaces.en);
  });

  // One test per namespace so a failure names the file rather than dumping
  // every key in the app.
  for (const ns of namespaces.en) {
    test(`${ns} has the same keys in en and ja`, () => {
      const en = baseKeys(read('en', ns));
      const ja = baseKeys(read('ja', ns));
      expect({ missingInJa: [...en].filter((k) => !ja.has(k)).sort() }).toEqual({
        missingInJa: [],
      });
      expect({ missingInEn: [...ja].filter((k) => !en.has(k)).sort() }).toEqual({
        missingInEn: [],
      });
    });
  }

  // Deliberately NOT "no value is empty": an empty string is a legitimate
  // translation for a column header that renders no text (the actions
  // column in the sent-notifications table is one). What is never
  // legitimate is English having copy where Japanese has none — that is a
  // translation someone dropped, and it renders as a blank label rather
  // than falling back, so it is invisible to the fallback path too.
  test('no Japanese value is blank where English has copy', () => {
    const blank: string[] = [];
    const strings = (v: unknown, path = '', acc = new Map<string, string>()) => {
      if (v !== null && typeof v === 'object') {
        for (const [k, nested] of Object.entries(v as Record<string, unknown>)) {
          strings(nested, path ? `${path}.${k}` : k, acc);
        }
      } else if (typeof v === 'string') {
        acc.set(path, v);
      }
      return acc;
    };
    for (const ns of namespaces.en) {
      const en = strings(read('en', ns));
      const ja = strings(read('ja', ns));
      for (const [k, v] of en) {
        if (v.trim() !== '' && ja.get(k)?.trim() === '') blank.push(`${ns}:${k}`);
      }
    }
    expect(blank).toEqual([]);
  });
});
