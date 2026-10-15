import { Link, MemoryRouter, Route, Routes, useLocation, useNavigate } from 'react-router-dom';

import { ConfirmDialogProvider } from '@/components/ui/confirm-dialog';
import { AuthProvider } from '@/lib/auth';

import { Activity } from './Activity';
import { Exec } from './Exec';
import { Run } from './Run';

function Navigation() {
  const location = useLocation();
  const navigate = useNavigate();
  return (
    <nav>
      <output data-testid="location">{location.pathname}{location.search}</output>
      <Link to="/activity?job_id=new-job&pc_id=new-pc&exec_id=new-exec">Other execution</Link>
      <button onClick={() => navigate(-1)}>Back</button>
      <button onClick={() => navigate(1)}>Forward</button>
    </nav>
  );
}

export function ExecutionActivityHost({ initial }: { initial: string }) {
  return (
    <MemoryRouter initialEntries={[initial]}>
      <AuthProvider>
        <ConfirmDialogProvider>
          <Navigation />
          <Routes>
            <Route path="/exec" element={<Exec />} />
            <Route path="/run" element={<Run />} />
            <Route path="/activity" element={<Activity />} />
          </Routes>
        </ConfirmDialogProvider>
      </AuthProvider>
    </MemoryRouter>
  );
}
