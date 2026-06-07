import { useTranslation } from 'react-i18next';

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { LANGUAGES, type LanguageCode } from '@/i18n';
import { useTheme, type Theme } from '@/lib/theme';

/// Operator preferences. Stored locally in localStorage; no backend round-trip.
export function Settings() {
  const { t, i18n } = useTranslation('settings');
  const { theme, setTheme } = useTheme();

  return (
    <div className="space-y-4">
      <h1 className="text-2xl font-bold">{t('title')}</h1>
      <p className="text-muted text-sm">{t('description')}</p>

      <div className="grid gap-4 md:grid-cols-2">
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

        <Card>
          <CardHeader>
            <CardTitle>{t('theme.title')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2">
            <Label htmlFor="theme-select">{t('theme.label')}</Label>
            <Select
              id="theme-select"
              value={theme}
              onChange={(e) => {
                setTheme(e.target.value as Theme);
              }}
            >
              <option value="system">{t('theme.options.system')}</option>
              <option value="light">{t('theme.options.light')}</option>
              <option value="dark">{t('theme.options.dark')}</option>
            </Select>
            <p className="text-muted text-xs">{t('theme.persistedHint')}</p>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
