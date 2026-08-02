import { Suspense } from 'react';
import { Navigate, Outlet, useLocation } from 'react-router-dom';
import { Loader2 } from 'lucide-react';

import { ErrorBoundary } from '@/components/ErrorBoundary';
import { groups, Sidebar } from '@/components/Sidebar';
import { useAuth } from '@/lib/auth';
import { featureForPathname } from '@/lib/features';

/// Auth gate + chrome for every `/api/*`-driven page. Renders the
/// left sidebar + an `<Outlet>` for the nested route only when the
/// user has a stored token; otherwise redirects to /login with the
/// current location stashed in `state.from` so the post-login
/// navigation can drop them back where they were.
export function ProtectedLayout() {
  const { isAuthenticated, mustChangePw, canSee, isRestricted, hasRole } = useAuth();
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

  // A restricted account's home: the first sidebar entry (in sidebar order)
  // its allow-list permits. Falls back to /change-password — reachable for
  // every account — when the allow-list holds no page feature at all.
  const firstAllowed = (() => {
    for (const g of groups) {
      for (const l of g.links) {
        if (l.adminOnly && !hasRole('admin')) continue;
        if (l.feature && canSee(l.feature)) return l.to;
      }
    }
    return '/change-password';
  })();

  // Per-account page visibility (#1008): a restricted account that types
  // (or bookmarks) the URL of a page it isn't allowed gets bounced to its
  // home (the dashboard for unrestricted accounts — nothing changes there).
  // `canSee`/`isRestricted` are optimistic while identity loads so
  // unrestricted users don't flash a redirect. The backend still 403s the
  // underlying data regardless.
  const feature = featureForPathname(location.pathname);
  if (feature && !canSee(feature)) {
    return <Navigate to={isRestricted ? firstAllowed : '/dashboard'} replace />;
  }

  // Commons routes (dashboard, agents, …) are open to unrestricted accounts
  // only: the backend 403s restricted accounts on the commons APIs, so
  // landing them there would render a page of errors. Send them to their
  // first allowed page instead. /change-password and /account stay
  // reachable — the mustChangePw trap above depends on the former, and the
  // latter is the self-service MFA page whose endpoints (/api/auth/mfa/*)
  // the backend allow-lists for restricted accounts too. (/login and
  // /password-setup are public routes outside this layout.)
  if (
    !feature &&
    isRestricted &&
    location.pathname !== '/change-password' &&
    location.pathname !== '/account'
  ) {
    return <Navigate to={firstAllowed} replace />;
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
          {/* #1215③: page components are React.lazy (route-level code
              splitting). Both boundaries sit INSIDE the chrome so a
              chunk load / failure touches only the content area — the
              sidebar must not unmount / flash on navigation. The
              ErrorBoundary is what keeps a stale-chunk import failure
              (old tab, redeployed backend) from white-screening the
              whole app — see ErrorBoundary.tsx. `key` ties the
              boundary to the route so a caught GENERIC error on one
              page doesn't trap the operator after they navigate away:
              the remount clears the error state and the new route
              renders. On the chunk-error path the remount just
              re-attempts the same failing import and re-shows the
              reload prompt — same UX, no trap either. */}
          <ErrorBoundary key={location.pathname}>
            <Suspense
              fallback={
                <div className="flex items-center justify-center py-24">
                  <Loader2 className="size-6 animate-spin text-muted-foreground" />
                </div>
              }
            >
              <Outlet />
            </Suspense>
          </ErrorBoundary>
        </div>
      </main>
    </div>
  );
}
