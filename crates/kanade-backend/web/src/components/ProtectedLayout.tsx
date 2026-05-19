import { Navigate, Outlet, useLocation } from 'react-router-dom';

import { Nav } from '@/components/Nav';
import { useAuth } from '@/lib/auth';

/// Auth gate + chrome for every `/api/*`-driven page. Renders the
/// nav + an `<Outlet>` for the nested route only when the user has
/// a stored token; otherwise redirects to /login with the current
/// location stashed in `state.from` so the post-login navigation
/// can drop them back where they were.
export function ProtectedLayout() {
  const { isAuthenticated } = useAuth();
  const location = useLocation();

  if (!isAuthenticated) {
    return <Navigate to="/login" state={{ from: location }} replace />;
  }

  return (
    <div className="min-h-screen flex flex-col">
      <Nav />
      <main className="flex-1 max-w-screen-2xl w-full mx-auto px-4 py-6">
        <Outlet />
      </main>
    </div>
  );
}
