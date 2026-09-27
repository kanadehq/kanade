import { useTranslation } from 'react-i18next';

import { Badge } from '@/components/ui/badge';
import { exitTone } from '@/lib/exitCode';

const VARIANT = { success: 'success', skipped: 'default', danger: 'danger' } as const;

/** A finished run's exit code: green for 0, neutral "skipped" when the
 *  agent didn't run the script (the result's `skipped` flag), red for
 *  everything else. */
export function ExitCodeBadge({ code, skipped }: { code: number; skipped?: boolean }) {
  const { t } = useTranslation('common');
  const tone = exitTone(code, skipped);
  if (tone === 'skipped') {
    return (
      <Badge variant={VARIANT.skipped} title={t('exitCode.skippedTitle')}>
        {t('exitCode.skipped', { code })}
      </Badge>
    );
  }
  return <Badge variant={VARIANT[tone]}>{code}</Badge>;
}
