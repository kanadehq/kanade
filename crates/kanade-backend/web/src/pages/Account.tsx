import { useTranslation } from 'react-i18next';

import { ChangePasswordCard } from '@/components/account/ChangePasswordCard';
import { MfaCard } from '@/components/account/MfaCard';

/**
 * Self-service account settings: two-factor auth + password, in one place
 * reached from a single nav entry. A normal in-app page (the ProtectedLayout
 * frame supplies the padding + max width), so it reads as a left-aligned
 * settings column instead of a lone card floating mid-screen on a wide
 * monitor.
 */
export function Account() {
  const { t } = useTranslation('common');
  return (
    <div className="max-w-2xl space-y-6">
      <h1 className="text-xl font-bold">{t('account.title')}</h1>
      <MfaCard />
      <ChangePasswordCard />
    </div>
  );
}
