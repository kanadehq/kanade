import { useTranslation } from 'react-i18next';

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { LANGUAGES, type LanguageCode } from '@/i18n';

/// Operator preferences. Single setting today (language), but the
/// page exists as the canonical home for any client-side toggle
/// that's per-operator rather than fleet-wide. Persistence is via
/// i18next's LanguageDetector → localStorage; no backend round-trip.
export function Settings() {
  const { t, i18n } = useTranslation('settings');

  return (
    <div className="space-y-4">
      <h1 className="text-2xl font-bold">{t('title')}</h1>
      <p className="text-muted text-sm">{t('description')}</p>

      <Card>
        <CardHeader>
          <CardTitle>{t('language.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          <Label htmlFor="language-select">{t('language.label')}</Label>
          <Select
            id="language-select"
            value={i18n.resolvedLanguage ?? 'en'}
            onChange={(e) => {
              const code = e.target.value as LanguageCode;
              void i18n.changeLanguage(code);
            }}
          >
            {LANGUAGES.map((l) => (
              <option key={l.code} value={l.code}>
                {l.label}
              </option>
            ))}
          </Select>
          <p className="text-muted text-xs">{t('language.persistedHint')}</p>
        </CardContent>
      </Card>
    </div>
  );
}
