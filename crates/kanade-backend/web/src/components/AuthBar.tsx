import { LogIn, LogOut } from 'lucide-react';
import { Link } from 'react-router-dom';

import { Button } from '@/components/ui/button';
import { useAuth } from '@/lib/auth';

/// Compact auth control in the top nav: a logout button when signed
/// in, a link to /login otherwise. The actual sign-in form lives on
/// the Login page now (replaces the v0.11.x modal dialog) so an
/// expired-token redirect lands somewhere meaningful.
export function AuthBar() {
  const { isAuthenticated, logout } = useAuth();

  if (isAuthenticated) {
    return (
      <div className="flex items-center gap-3">
        <span className="text-xs text-muted hidden sm:inline">auth: signed in</span>
        <Button variant="secondary" size="sm" onClick={logout}>
          <LogOut className="size-3.5" />
          logout
        </Button>
      </div>
    );
  }

  return (
    <Button variant="secondary" size="sm" asChild>
      <Link to="/login">
        <LogIn className="size-3.5" />
        login
      </Link>
    </Button>
  );
}
