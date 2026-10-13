import { useEffect, useState } from 'react';

import { apiFetch } from '@/lib/api';

/**
 * Whether the backend has passkeys configured (`[server.webauthn]`). The
 * probe is public so the login screen can use it before there is a session.
 * `false` until answered, and on any error, so passkey UI never flashes for a
 * deployment that has not enabled it.
 */
export function usePasskeyEnabled(): boolean {
  const [enabled, setEnabled] = useState(false);
  useEffect(() => {
    let cancelled = false;
    apiFetch<{ enabled: boolean }>('/api/auth/passkey/config')
      .then((c) => {
        if (!cancelled) setEnabled(c.enabled === true);
      })
      .catch(() => {
        if (!cancelled) setEnabled(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);
  return enabled;
}
