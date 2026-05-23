/**
 * i18next bootstrap. Initialised once at module load (imported from
 * main.tsx before React renders) so every component that calls
 * `useTranslation()` finds the instance already configured.
 *
 * Languages: English (default) + Japanese. Detection is client-side
 * only — the operator's preference lives in
 * `localStorage.kanade_lang`, and the LanguageDetector reads it
 * automatically. No backend round-trip; single-operator fleets
 * don't justify a /api/preferences endpoint.
 *
 * Namespaces: one per page + `common` for nav / generic buttons.
 * Catalogs are picked up via `import.meta.glob({ eager: true })` so
 * dropping a new `locales/{en,ja}/{namespace}.json` is enough — no
 * need to also touch this file. Eager mode inlines the JSON into
 * the main chunk; the entire catalog is small (KB-scale) and gzip
 * handles the rest, so a first paint never flashes an English
 * string while a lazy-loaded chunk catches up.
 */

import i18n from 'i18next';
import LanguageDetector from 'i18next-browser-languagedetector';
import { initReactI18next } from 'react-i18next';

export const LANGUAGES = [
  { code: 'en', label: 'English' },
  { code: 'ja', label: '日本語' },
] as const;

export type LanguageCode = (typeof LANGUAGES)[number]['code'];

type CatalogModule = { default: Record<string, unknown> };

// Pull every `locales/{lang}/{namespace}.json` into a namespace map.
// `import.meta.glob` with `eager: true` is resolved at build time —
// Vite emits static imports, so there's no runtime fetch and the
// shape is identical to hand-written imports.
function loadCatalog(modules: Record<string, CatalogModule>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [path, mod] of Object.entries(modules)) {
    // Path looks like `/src/locales/en/common.json`; the namespace
    // is the basename without the extension.
    const name = path.split('/').pop()?.replace(/\.json$/, '');
    if (name) out[name] = mod.default;
  }
  return out;
}

const enCatalogs = import.meta.glob<CatalogModule>('@/locales/en/*.json', { eager: true });
const jaCatalogs = import.meta.glob<CatalogModule>('@/locales/ja/*.json', { eager: true });

void i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources: {
      en: loadCatalog(enCatalogs),
      ja: loadCatalog(jaCatalogs),
    },
    fallbackLng: 'en',
    supportedLngs: ['en', 'ja'],
    interpolation: {
      // React already escapes — i18next's default HTML escape would
      // double-encode things like `&` inside translation strings.
      escapeValue: false,
    },
    detection: {
      // Read the operator's preference from localStorage first; fall
      // back to the navigator language; persist explicit picks to
      // localStorage so the next session opens in the right locale.
      order: ['localStorage', 'navigator'],
      lookupLocalStorage: 'kanade_lang',
      caches: ['localStorage'],
    },
  });

export default i18n;
