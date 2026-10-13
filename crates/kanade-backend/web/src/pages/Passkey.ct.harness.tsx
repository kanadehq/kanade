// Mount hosts for the passkey browser tests. Like the other `*.harness.tsx`
// files, these exist because Playwright CT can only mount what it can import.
//
// The real components need the app's providers: a router (Login navigates
// after sign-in), AuthProvider (identity + the `mfa_enabled` flag), and a
// Toaster (the Account card reports through toasts).
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { Toaster } from 'sonner';

import { PasskeyCard } from '@/components/account/PasskeyCard';
import { AuthProvider, useAuth } from '@/lib/auth';
import { Login } from './Login';

// Stand-in for ProtectedLayout's forced-password-change trap: the real app
// bounces a `must_change_pw` session from any page to /change-password.
function Home() {
  const { mustChangePw } = useAuth();
  return <p>{mustChangePw ? 'change password gate' : 'dashboard reached'}</p>;
}

export function LoginHost() {
  return (
    <MemoryRouter initialEntries={['/login']}>
      <AuthProvider>
        <Routes>
          <Route path="/login" element={<Login />} />
          <Route path="/dashboard" element={<Home />} />
          <Route path="/change-password" element={<p>change password gate</p>} />
        </Routes>
      </AuthProvider>
    </MemoryRouter>
  );
}

export function CardHost() {
  return (
    <MemoryRouter>
      <AuthProvider>
        <Toaster />
        <div className="w-[640px] p-4">
          <PasskeyCard />
        </div>
      </AuthProvider>
    </MemoryRouter>
  );
}
