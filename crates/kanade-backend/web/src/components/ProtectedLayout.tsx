import { Navigate, Outlet, useLocation } from 'react-router-dom';

import { Sidebar } from '@/components/Sidebar';
import { useAuth } from '@/lib/auth';
import { featureForPathname } from '@/lib/features';

/// Auth gate + chrome for every `/api/*`-driven page. Renders the
/// left sidebar + an `<Outlet>` for the nested route only when the
/// user has a stored token; otherwise redirects to /login with the
/// current location stashed in `state.from` so the post-login
/// navigation can drop them back where they were.
export function ProtectedLayout() {
  const { isAuthenticated, mustChangePw, canSee } = useAuth();
  const location = useLocation();

  if (!isAuthenticated) {
    return <Navigate to="/login" state={{ from: location }} replace />;
  }

  // Forced password change (initial / admin reset): trap the user on
  // /change-password until `me.must_change_pw` clears. The backend also
  // refuses writes for these accounts, but the SPA gate makes it explicit.
  if (mustChangePw && location.pathname !== '/change-password') {
    return <Navigate to="/change-password" replace />;
  }

  // Per-account page visibility (#1008): a restricted account that types
  // (or bookmarks) the URL of a page it isn't allowed gets bounced to the
  // dashboard. Commons/baseline routes map to `null` and are always allowed;
  // `canSee` is optimistic while identity loads so unrestricted users don't
  // flash a redirect. The backend still 403s the underlying data regardless.
  const feature = featureForPathname(location.pathname);
  if (feature && !canSee(feature)) {
    return <Navigate to="/dashboard" replace />;
  }

  return (
    <div className="min-h-screen">
      <Sidebar />
      {/* md:pl-56 mirrors `SIDEBAR_WIDTH_CLASS = 'w-56'` in
          Sidebar.tsx so the page content starts at the right edge
          of the fixed aside on desktop. Mobile keeps full width
          with the slim hamburger row on top. Tailwind JIT means we
          can't share a single source for both values — update both
          together when changing the sidebar width. */}
      <main className="md:pl-56">
        <div className="max-w-screen-2xl mx-auto px-4 py-6">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
