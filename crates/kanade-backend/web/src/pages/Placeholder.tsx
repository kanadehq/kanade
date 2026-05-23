import { Construction } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

/// Catch-all route renderer. App.tsx wires this at `path="*"` with a
/// `name="Not Found"` prop today; future routes that are wired up
/// before their real page component lands can mount this with their
/// feature name so the operator sees "Foo — coming soon" rather than
/// a misleading "Not Found".
export function Placeholder({ name = 'Page' }: { name?: string } = {}) {
  const { t } = useTranslation('common');
  return (
    <Card>
      <CardHeader className="items-center text-center">
        <Construction className="size-10 text-amber" />
        <CardTitle>{t('comingSoon.title', { name })}</CardTitle>
      </CardHeader>
      <CardContent className="text-center text-muted">{t('comingSoon.description')}</CardContent>
    </Card>
  );
}
