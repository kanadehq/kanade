import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';

import { ProtectedLayout } from '@/components/ProtectedLayout';
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
import { Results } from '@/pages/Results';
import { Rollout } from '@/pages/Rollout';
import { Run } from '@/pages/Run';
import { Schedules } from '@/pages/Schedules';

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
          <Routes>
            {/* Public route — the only thing reachable when not signed in. */}
            <Route path="/login" element={<Login />} />

            {/* Everything else lives under ProtectedLayout's auth gate. */}
            <Route element={<ProtectedLayout />}>
              <Route path="/" element={<Navigate to="/dashboard" replace />} />
              <Route path="/dashboard" element={<Dashboard />} />
              <Route path="/agents" element={<Agents />} />
              <Route path="/run" element={<Run />} />
              <Route path="/results" element={<Results />} />
              <Route path="/audit" element={<Audit />} />
              <Route path="/logs" element={<Logs />} />
              <Route path="/inventory" element={<Inventory />} />
              <Route path="/jobs" element={<Jobs />} />
              <Route path="/schedules" element={<Schedules />} />
              <Route path="/exec" element={<Exec />} />
              <Route path="/rollout" element={<Rollout />} />
              <Route path="/config" element={<Config />} />
              <Route path="/jetstream" element={<JetStream />} />
              <Route path="*" element={<Placeholder name="Not Found" />} />
            </Route>
          </Routes>
        </AuthProvider>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
