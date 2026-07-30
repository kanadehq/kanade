import { LogIn, LogOut, UserCog } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router-dom';

import { Button } from '@/components/ui/button';
import { useAuth } from '@/lib/auth';

/// Compact auth control in the top nav: a logout button when signed
/// in, a link to /login otherwise. The actual sign-in form lives on
/// the Login page now (replaces the v0.11.x modal dialog) so an
/// expired-token redirect lands somewhere meaningful.
export function AuthBar() {
  const { isAuthenticated, logout, username, role } = useAuth();
  const { t } = useTranslation('common');

  if (isAuthenticated) {
    return (
      // `min-w-0 flex-1` on the identity + `shrink-0` on the actions keeps
      // the long username truncating instead of shoving the buttons off the
      // narrow (w-56) sidebar and colliding with them.
      <div className="flex items-center justify-between gap-2">
        <div className="min-w-0 flex-1">
          {/* Identity + role so the operator always knows which
              account / privilege level they're acting as. */}
          <div className="text-xs font-medium truncate">{username ?? t('auth.signedIn')}</div>
          {role && <div className="text-[10px] uppercase tracking-wide text-muted">{role}</div>}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {/* One entry to the Account page (MFA + password) instead of the
              two icons that used to crowd the account name. */}
          <Button variant="ghost" size="icon" asChild title={t('auth.account')}>
            <Link to="/account" aria-label={t('auth.account')}>
              <UserCog className="size-3.5" />
            </Link>
          </Button>
          <Button variant="secondary" size="sm" onClick={logout}>
            <LogOut className="size-3.5" />
            {t('auth.logout')}
          </Button>
        </div>
      </div>
    );
  }

  return (
    <Button variant="secondary" size="sm" asChild>
      <Link to="/login">
        <LogIn className="size-3.5" />
        {t('auth.login')}
      </Link>
    </Button>
  );
}
