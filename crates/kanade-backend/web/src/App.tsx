import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { Toaster } from 'sonner';

import { ProtectedLayout } from '@/components/ProtectedLayout';
import { ConfirmDialogProvider } from '@/components/ui/confirm-dialog';
import { AuthProvider } from '@/lib/auth';
import { Agents } from '@/pages/Agents';
import { Audit } from '@/pages/Audit';
import { Config } from '@/pages/Config';
import { Dashboard } from '@/pages/Dashboard';
import { Exec } from '@/pages/Exec';
import { Inventory } from '@/pages/Inventory';
import { JetStream } from '@/pages/JetStream';
import { Jobs } from '@/pages/Jobs';
import { Login } from '@/pages/Login';
import { Logs } from '@/pages/Logs';
import { Placeholder } from '@/pages/Placeholder';
import { Activity } from '@/pages/Activity';
import { ResultDetail } from '@/pages/ResultDetail';
import { Rollout } from '@/pages/Rollout';
import { Run } from '@/pages/Run';
import { Schedules } from '@/pages/Schedules';
import { InventorySearch } from '@/pages/Search';

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
      <BrowserRouter>
        <AuthProvider>
          <ConfirmDialogProvider>
            <Toaster
              position="bottom-right"
              theme="dark"
              richColors
              closeButton
              toastOptions={{ duration: 4000 }}
            />
          <Routes>
            {/* Public route — the only thing reachable when not signed in. */}
            <Route path="/login" element={<Login />} />

            {/* Everything else lives under ProtectedLayout's auth gate. */}
            <Route element={<ProtectedLayout />}>
              <Route path="/" element={<Navigate to="/dashboard" replace />} />
              <Route path="/dashboard" element={<Dashboard />} />
              <Route path="/agents" element={<Agents />} />
              <Route path="/run" element={<Run />} />
              <Route path="/activity" element={<Activity />} />
              <Route path="/activity/:resultId" element={<ResultDetail />} />
              {/* Back-compat: any pre-rename /results bookmark redirects
                  to /activity. Detail-route param is dropped — operator
                  scrolls/filters to the row on the unified Activity
                  page. Cheap to add, helps anyone who linked to a
                  specific run while we were on the old name. */}
              <Route path="/results" element={<Navigate to="/activity" replace />} />
              <Route path="/results/*" element={<Navigate to="/activity" replace />} />
              <Route path="/audit" element={<Audit />} />
              <Route path="/logs" element={<Logs />} />
              <Route path="/inventory" element={<Inventory />} />
              <Route path="/inventory/search" element={<InventorySearch />} />
              <Route path="/jobs" element={<Jobs />} />
              <Route path="/schedules" element={<Schedules />} />
              <Route path="/exec" element={<Exec />} />
              <Route path="/rollout" element={<Rollout />} />
              <Route path="/config" element={<Config />} />
              <Route path="/jetstream" element={<JetStream />} />
              <Route path="*" element={<Placeholder name="Not Found" />} />
            </Route>
          </Routes>
          </ConfirmDialogProvider>
        </AuthProvider>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
