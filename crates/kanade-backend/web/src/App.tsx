import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { Toaster } from 'sonner';

import { ProtectedLayout } from '@/components/ProtectedLayout';
import { ConfirmDialogProvider } from '@/components/ui/confirm-dialog';
import { AuthProvider } from '@/lib/auth';
import { Accounts } from '@/pages/Accounts';
import { AgentDetail } from '@/pages/AgentDetail';
import { Agents } from '@/pages/Agents';
import { Apps } from '@/pages/Apps';
import { ChangePassword } from '@/pages/ChangePassword';
import { Audit } from '@/pages/Audit';
import { Compliance } from '@/pages/Compliance';
import { Config } from '@/pages/Config';
import { Dashboard } from '@/pages/Dashboard';
import { Events } from '@/pages/Events';
import { Exec } from '@/pages/Exec';
import { Groups } from '@/pages/Groups';
import { Inventory } from '@/pages/Inventory';
import { JetStream } from '@/pages/JetStream';
import { Jobs } from '@/pages/Jobs';
import { Login } from '@/pages/Login';
import { Logs } from '@/pages/Logs';
import { Notifications } from '@/pages/Notifications';
import { Placeholder } from '@/pages/Placeholder';
import { Activity } from '@/pages/Activity';
import { ResultDetail } from '@/pages/ResultDetail';
import { Rollout } from '@/pages/Rollout';
import { Run } from '@/pages/Run';
import { Schedules } from '@/pages/Schedules';
import { Settings } from '@/pages/Settings';
import { ThemeProvider, useTheme } from '@/lib/theme';

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
            {/* Public route — the only thing reachable when not signed in. */}
            <Route path="/login" element={<Login />} />

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
              <Route path="/jobs" element={<Jobs />} />
              <Route path="/schedules" element={<Schedules />} />
              <Route path="/exec" element={<Exec />} />
              <Route path="/rollout" element={<Rollout />} />
              <Route path="/apps" element={<Apps />} />
              <Route path="/groups" element={<Groups />} />
              <Route path="/notifications" element={<Notifications />} />
              <Route path="/config" element={<Config />} />
              <Route path="/jetstream" element={<JetStream />} />
              <Route path="/settings" element={<Settings />} />
              {/* RBAC: account management (admin-gated inside the page +
                  backend) and self-service forced password change. */}
              <Route path="/accounts" element={<Accounts />} />
              <Route path="/change-password" element={<ChangePassword />} />
              <Route path="*" element={<Placeholder name="Not Found" />} />
            </Route>
          </Routes>
        </ConfirmDialogProvider>
      </AuthProvider>
    </BrowserRouter>
  );
}
