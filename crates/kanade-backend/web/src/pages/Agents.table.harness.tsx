import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';

import { ConfirmDialogProvider } from '@/components/ui/confirm-dialog';
import { AuthProvider } from '@/lib/auth';
import { Agents } from './Agents';

function Location() {
  const location = useLocation();
  return <output data-testid="location">{location.pathname}{location.search}</output>;
}

export function AgentsTableHarness({ initial = '/agents' }: { initial?: string }) {
  return (
    <MemoryRouter initialEntries={[initial]}>
      <Location />
      <AuthProvider>
        <ConfirmDialogProvider>
          <Routes>
            <Route path="/agents" element={<Agents />} />
          </Routes>
        </ConfirmDialogProvider>
      </AuthProvider>
    </MemoryRouter>
  );
}
