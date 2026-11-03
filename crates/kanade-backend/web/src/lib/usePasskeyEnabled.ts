import { useQuery } from '@tanstack/react-query';
import { apiFetch } from '@/lib/api';

/** Public feature probe, shared by login, account management and settings. */
export function usePasskeyEnabled(): boolean {
  const query = useQuery({
    queryKey: ['passkey-config'],
    queryFn: () => apiFetch<{ enabled: boolean }>('/api/auth/passkey/config'),
    staleTime: 0,
    retry: false,
  });
  return !query.isError && query.data?.enabled === true;
}
