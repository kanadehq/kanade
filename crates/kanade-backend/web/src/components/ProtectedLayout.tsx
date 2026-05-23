import { Navigate, Outlet, useLocation } from 'react-router-dom';

import { Sidebar } from '@/components/Sidebar';
import { useAuth } from '@/lib/auth';

/// Auth gate + chrome for every `/api/*`-driven page. Renders the
/// left sidebar + an `<Outlet>` for the nested route only when the
/// user has a stored token; otherwise redirects to /login with the
/// current location stashed in `state.from` so the post-login
/// navigation can drop them back where they were.
export function ProtectedLayout() {
  const { isAuthenticated } = useAuth();
  const location = useLocation();

  if (!isAuthenticated) {
    return <Navigate to="/login" state={{ from: location }} replace />;
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
