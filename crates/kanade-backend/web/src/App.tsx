import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { lazy } from 'react';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { Toaster } from 'sonner';

import { ProtectedLayout } from '@/components/ProtectedLayout';
import { ConfirmDialogProvider } from '@/components/ui/confirm-dialog';
import { AuthProvider } from '@/lib/auth';
import { Login } from '@/pages/Login';
import { PasswordSetup } from '@/pages/PasswordSetup';
import { Placeholder } from '@/pages/Placeholder';
import { ThemeProvider, useTheme } from '@/lib/theme';

// #1215③: route-level code splitting. Every authenticated page is a
// lazy chunk so the entry bundle no longer carries the whole app —
// most importantly the heavy single-route deps (monaco-editor via the
// YAML editor, recharts via Dashboard/Analytics, marked+dompurify via
// Notifications, qrcode via Account). Vite splits those automatically
// once no eager importer chains them into the entry chunk. The
// Suspense boundary lives in ProtectedLayout (around <Outlet/>) so
// chunk loads swap only the content area, not the chrome.
//
// Login / PasswordSetup / Placeholder stay eager: the two public
// routes ARE the cold-load target for an unauthenticated session, and
// Placeholder is a one-liner.
const Accounts = lazy(() => import('@/pages/Accounts').then((m) => ({ default: m.Accounts })));
const AgentDetail = lazy(() => import('@/pages/AgentDetail').then((m) => ({ default: m.AgentDetail })));
const AgentInstall = lazy(() =>
  import('@/pages/AgentInstall').then((m) => ({ default: m.AgentInstall })),
);
const Agents = lazy(() => import('@/pages/Agents').then((m) => ({ default: m.Agents })));
const Apps = lazy(() => import('@/pages/Apps').then((m) => ({ default: m.Apps })));
const ChangePassword = lazy(() =>
  import('@/pages/ChangePassword').then((m) => ({ default: m.ChangePassword })),
);
const Audit = lazy(() => import('@/pages/Audit').then((m) => ({ default: m.Audit })));
const Collect = lazy(() => import('@/pages/Collect').then((m) => ({ default: m.Collect })));
const Compliance = lazy(() => import('@/pages/Compliance').then((m) => ({ default: m.Compliance })));
const Config = lazy(() => import('@/pages/Config').then((m) => ({ default: m.Config })));
const Dashboard = lazy(() => import('@/pages/Dashboard').then((m) => ({ default: m.Dashboard })));
const Events = lazy(() => import('@/pages/Events').then((m) => ({ default: m.Events })));
const Exec = lazy(() => import('@/pages/Exec').then((m) => ({ default: m.Exec })));
const Groups = lazy(() => import('@/pages/Groups').then((m) => ({ default: m.Groups })));
const Inventory = lazy(() => import('@/pages/Inventory').then((m) => ({ default: m.Inventory })));
const JetStream = lazy(() => import('@/pages/JetStream').then((m) => ({ default: m.JetStream })));
const Jobs = lazy(() => import('@/pages/Jobs').then((m) => ({ default: m.Jobs })));
const Logs = lazy(() => import('@/pages/Logs').then((m) => ({ default: m.Logs })));
const NotificationDetail = lazy(() =>
  import('@/pages/NotificationDetail').then((m) => ({ default: m.NotificationDetail })),
);
const Notifications = lazy(() =>
  import('@/pages/Notifications').then((m) => ({ default: m.Notifications })),
);
const Activity = lazy(() => import('@/pages/Activity').then((m) => ({ default: m.Activity })));
const ResultDetail = lazy(() =>
  import('@/pages/ResultDetail').then((m) => ({ default: m.ResultDetail })),
);
const RemoteScreen = lazy(() =>
  import('@/pages/RemoteScreen').then((m) => ({ default: m.RemoteScreen })),
);
const Rollout = lazy(() => import('@/pages/Rollout').then((m) => ({ default: m.Rollout })));
const Run = lazy(() => import('@/pages/Run').then((m) => ({ default: m.Run })));
const Analytics = lazy(() => import('@/pages/Analytics').then((m) => ({ default: m.Analytics })));
const Schedules = lazy(() => import('@/pages/Schedules').then((m) => ({ default: m.Schedules })));
const Account = lazy(() => import('@/pages/Account').then((m) => ({ default: m.Account })));
const Settings = lazy(() => import('@/pages/Settings').then((m) => ({ default: m.Settings })));
const Views = lazy(() => import('@/pages/Views').then((m) => ({ default: m.Views })));

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // Stale immediately so list pages re-fetch on focus, but
      // keep cached data around for 30s so navigating back from
      // a detail page renders instantly.
      staleTime: 0,
      gcTime: 30_000,
      retry: 1,
      refetchOnWindowFocus: true,
    },
  },
});

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <AppContent />
      </ThemeProvider>
    </QueryClientProvider>
  );
}

function ThemedToaster() {
  const { theme } = useTheme();
  return (
    <Toaster
      position="bottom-right"
      // Honour the operator's theme selection (light, dark, system).
      theme={theme}
      richColors
      closeButton
      toastOptions={{ duration: 4000 }}
    />
  );
}

function AppContent() {
  return (
    <BrowserRouter>
      <AuthProvider>
        <ConfirmDialogProvider>
          <ThemedToaster />
          <Routes>
            {/* Public routes — reachable without a session. The
                password-setup link's one-time token IS the credential, so
                it sits outside the auth gate like /login. */}
            <Route path="/login" element={<Login />} />
            <Route path="/password-setup/:token" element={<PasswordSetup />} />

            {/* Everything else lives under ProtectedLayout's auth gate. */}
            <Route element={<ProtectedLayout />}>
              <Route path="/" element={<Navigate to="/dashboard" replace />} />
              <Route path="/dashboard" element={<Dashboard />} />
              <Route path="/agents" element={<Agents />} />
              <Route path="/agents/:pcId" element={<AgentDetail />} />
              <Route path="/run" element={<Run />} />
              <Route path="/activity" element={<Activity />} />
              <Route path="/activity/:resultId" element={<ResultDetail />} />
              <Route path="/events" element={<Events />} />
              {/* Back-compat: any pre-rename /results bookmark redirects
                  to /activity. Detail-route param is dropped — operator
                  scrolls/filters to the row on the unified Activity
                  page. Cheap to add, helps anyone who linked to a
                  specific run while we were on the old name. */}
              <Route path="/results" element={<Navigate to="/activity" replace />} />
              <Route path="/results/*" element={<Navigate to="/activity" replace />} />
              <Route path="/audit" element={<Audit />} />
              <Route path="/logs" element={<Logs />} />
              {/* Both paths render the Inventory page; it reads the
                  pathname to pick the active tab (overview vs the
                  embedded fleet-search panel). /inventory/search stays
                  a real deep link so bookmarks and the search-result
                  row links keep working. */}
              <Route path="/inventory" element={<Inventory />} />
              <Route path="/inventory/search" element={<Inventory />} />
              <Route path="/compliance" element={<Compliance />} />
              <Route path="/collect" element={<Collect />} />
              <Route path="/analytics" element={<Analytics />} />
              <Route path="/jobs" element={<Jobs />} />
              <Route path="/schedules" element={<Schedules />} />
              <Route path="/views" element={<Views />} />
              <Route path="/exec" element={<Exec />} />
              <Route path="/rollout" element={<Rollout />} />
              <Route path="/agent-install" element={<AgentInstall />} />
              <Route path="/apps" element={<Apps />} />
              {/* #1140: reached from a PC's detail page, not the sidebar — the
                  viewer is meaningless without a pc_id. `featureForPathname`
                  keys off the first segment, so this is gated as `remote`. */}
              <Route path="/remote/:pcId" element={<RemoteScreen />} />
              <Route path="/groups" element={<Groups />} />
              <Route path="/notifications" element={<Notifications />} />
              <Route path="/notifications/:id" element={<NotificationDetail />} />
              <Route path="/config" element={<Config />} />
              <Route path="/jetstream" element={<JetStream />} />
              <Route path="/settings" element={<Settings />} />
              {/* RBAC: account management (admin-gated inside the page +
                  backend) and self-service forced password change. */}
              <Route path="/accounts" element={<Accounts />} />
              <Route path="/change-password" element={<ChangePassword />} />
              <Route path="/account" element={<Account />} />
              {/* MFA + password moved into one Account page; keep the old
                  path working for bookmarks. */}
              <Route path="/security" element={<Navigate to="/account" replace />} />
              <Route path="*" element={<Placeholder name="Not Found" />} />
            </Route>
          </Routes>
        </ConfirmDialogProvider>
      </AuthProvider>
    </BrowserRouter>
  );
}
