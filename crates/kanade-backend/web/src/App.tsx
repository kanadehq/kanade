import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';

import { Nav } from '@/components/Nav';
import { AuthProvider } from '@/lib/auth';
import { Agents } from '@/pages/Agents';
import { Placeholder } from '@/pages/Placeholder';

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
                <Route path="/" element={<Navigate to="/agents" replace />} />
                <Route path="/agents" element={<Agents />} />
                <Route path="/run" element={<Placeholder name="Run" />} />
                <Route path="/results" element={<Placeholder name="Results" />} />
                <Route path="/audit" element={<Placeholder name="Audit" />} />
                <Route path="/schedules" element={<Placeholder name="Schedules" />} />
                <Route path="/deploy" element={<Placeholder name="Deploy" />} />
                <Route path="/config" element={<Placeholder name="Config" />} />
                <Route path="/jetstream" element={<Placeholder name="JetStream" />} />
                <Route path="*" element={<Placeholder name="Not Found" />} />
              </Routes>
            </main>
          </div>
        </BrowserRouter>
      </AuthProvider>
    </QueryClientProvider>
  );
}
