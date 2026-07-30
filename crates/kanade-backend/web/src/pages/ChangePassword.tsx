import { useNavigate } from 'react-router-dom';

import { ChangePasswordCard } from '@/components/account/ChangePasswordCard';

/**
 * The `must_change_pw` gate: a freshly-seeded / reset account is forced
 * here (ProtectedLayout redirects to `/change-password` until the flag
 * clears) before it can use the app. Rendered standalone + centered like
 * the Login screen — it's a pre-app gate, not a settings page — and reuses
 * the same {@link ChangePasswordCard} the Account page uses. On success the
 * card has already re-fetched identity (clearing the gate); we navigate on.
 */
export function ChangePassword() {
  const navigate = useNavigate();
  return (
    <main className="min-h-screen flex items-center justify-center px-4 py-8 bg-bg">
      <div className="w-full max-w-md">
        <ChangePasswordCard onSuccess={() => navigate('/dashboard', { replace: true })} />
      </div>
    </main>
  );
}
