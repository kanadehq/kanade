import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';

import { Nav } from '@/components/Nav';
import { AuthProvider } from '@/lib/auth';
import { Agents } from '@/pages/Agents';
import { Audit } from '@/pages/Audit';
import { Config } from '@/pages/Config';
import { Dashboard } from '@/pages/Dashboard';
import { Deploy } from '@/pages/Deploy';
import { JetStream } from '@/pages/JetStream';
import { Placeholder } from '@/pages/Placeholder';
import { Results } from '@/pages/Results';
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
      <AuthProvider>
        <BrowserRouter>
          <div className="min-h-screen flex flex-col">
            <Nav />
            <main className="flex-1 max-w-7xl w-full mx-auto px-6 py-8">
              <Routes>
                <Route path="/" element={<Navigate to="/dashboard" replace />} />
                <Route path="/dashboard" element={<Dashboard />} />
                <Route path="/agents" element={<Agents />} />
                <Route path="/run" element={<Run />} />
                <Route path="/results" element={<Results />} />
                <Route path="/audit" element={<Audit />} />
                <Route path="/schedules" element={<Schedules />} />
                <Route path="/deploy" element={<Deploy />} />
                <Route path="/config" element={<Config />} />
                <Route path="/jetstream" element={<JetStream />} />
                <Route path="*" element={<Placeholder name="Not Found" />} />
              </Routes>
            </main>
          </div>
        </BrowserRouter>
      </AuthProvider>
    </QueryClientProvider>
  );
}
