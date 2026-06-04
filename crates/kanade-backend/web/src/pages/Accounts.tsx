import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { UserPlus } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth, type Role } from '@/lib/auth';

type Account = {
  username: string;
  role: Role;
  disabled: number;
  must_change_pw: number;
  created_at: string;
  updated_at: string;
};

const ROLES: Role[] = ['viewer', 'operator', 'admin'];

export function Accounts() {
  const { t } = useTranslation('accounts');
  const { hasRole } = useAuth();
  const qc = useQueryClient();
  const confirm = useConfirm();

  const accounts = useQuery({
    queryKey: ['accounts'],
    queryFn: () => apiFetch<Account[]>('/api/accounts'),
    enabled: hasRole('admin'),
  });

  // create form
  const [newUser, setNewUser] = useState('');
  const [newPw, setNewPw] = useState('');
  const [newRole, setNewRole] = useState<Role>('viewer');
  // reset-password dialog
  const [resetFor, setResetFor] = useState<string | null>(null);
  const [resetPw, setResetPw] = useState('');

  const invalidate = () => qc.invalidateQueries({ queryKey: ['accounts'] });
  const onError = (err: unknown) => toast.error(formatError(err));

  const create = useMutation({
    mutationFn: () =>
      apiFetch('/api/accounts', {
        method: 'POST',
        body: JSON.stringify({ username: newUser.trim(), password: newPw, role: newRole }),
      }),
    onSuccess: () => {
      toast.success(t('toast.created', { username: newUser.trim() }));
      setNewUser('');
      setNewPw('');
      setNewRole('viewer');
      invalidate();
    },
    onError,
  });

  const patch = useMutation({
    mutationFn: (v: { username: string; body: Record<string, unknown> }) =>
      apiFetch(`/api/accounts/${encodeURIComponent(v.username)}`, {
        method: 'PATCH',
        body: JSON.stringify(v.body),
      }),
    onSuccess: () => invalidate(),
    onError,
  });

  const del = useMutation({
    mutationFn: (username: string) =>
      apiFetch(`/api/accounts/${encodeURIComponent(username)}`, { method: 'DELETE' }),
    onSuccess: () => {
      toast.success(t('toast.deleted'));
      invalidate();
    },
    onError,
  });

  if (!hasRole('admin')) {
    return (
      <div className="p-6">
        <p className="text-muted">{t('forbidden')}</p>
      </div>
    );
  }

  return (
    <div className="p-4 md:p-6 space-y-6 max-w-5xl">
      <header>
        <h1 className="text-2xl font-bold">{t('title')}</h1>
        <p className="text-muted text-sm">{t('subtitle')}</p>
      </header>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('createTitle')}</CardTitle>
        </CardHeader>
        <CardContent>
          <form
            className="flex flex-wrap items-end gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              if (!newUser.trim() || newPw.length < 8) {
                toast.error(t('createHint'));
                return;
              }
              create.mutate();
            }}
          >
            <div className="space-y-1">
              <Label htmlFor="new-username">{t('username')}</Label>
              <Input
                id="new-username"
                value={newUser}
                onChange={(e) => setNewUser(e.target.value)}
                className="w-44"
              />
            </div>
            <div className="space-y-1">
              <Label htmlFor="new-password">{t('password')}</Label>
              <Input
                id="new-password"
                type="password"
                autoComplete="new-password"
                value={newPw}
                onChange={(e) => setNewPw(e.target.value)}
                className="w-44"
              />
            </div>
            <div className="space-y-1">
              <Label htmlFor="new-role">{t('role')}</Label>
              <Select
                id="new-role"
                value={newRole}
                onChange={(e) => setNewRole(e.target.value as Role)}
                className="w-36"
              >
                {ROLES.map((r) => (
                  <option key={r} value={r}>
                    {t(`roles.${r}`)}
                  </option>
                ))}
              </Select>
            </div>
            <Button type="submit" disabled={create.isPending}>
              <UserPlus className="size-4 mr-2" />
              {t('submit')}
            </Button>
          </form>
          <p className="mt-2 text-xs text-muted">{t('createHint')}</p>
        </CardContent>
      </Card>

      {accounts.isError && <p className="text-red-500 text-sm">{formatError(accounts.error)}</p>}

      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>{t('username')}</TableHead>
            <TableHead>{t('role')}</TableHead>
            <TableHead>{t('status')}</TableHead>
            <TableHead>{t('created')}</TableHead>
            <TableHead className="text-right">{t('actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {(accounts.data ?? []).map((a) => (
            <TableRow key={a.username}>
              <TableCell className="font-medium">
                {a.username}
                {a.must_change_pw === 1 && (
                  <Badge variant="amber" className="ml-2">
                    {t('mustChange')}
                  </Badge>
                )}
              </TableCell>
              <TableCell>
                <Select
                  value={a.role}
                  className="w-32"
                  onChange={(e) =>
                    patch.mutate({ username: a.username, body: { role: e.target.value } })
                  }
                >
                  {ROLES.map((r) => (
                    <option key={r} value={r}>
                      {t(`roles.${r}`)}
                    </option>
                  ))}
                </Select>
              </TableCell>
              <TableCell>
                {a.disabled === 1 ? (
                  <Badge variant="danger">{t('disabled')}</Badge>
                ) : (
                  <Badge variant="success">{t('enabled')}</Badge>
                )}
              </TableCell>
              <TableCell className="text-muted text-xs">{a.created_at}</TableCell>
              <TableCell className="text-right space-x-2 whitespace-nowrap">
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => {
                    setResetFor(a.username);
                    setResetPw('');
                  }}
                >
                  {t('resetPassword')}
                </Button>
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() =>
                    patch.mutate({
                      username: a.username,
                      body: { disabled: a.disabled !== 1 },
                    })
                  }
                >
                  {a.disabled === 1 ? t('enable') : t('disable')}
                </Button>
                <Button
                  variant="danger"
                  size="sm"
                  onClick={async () => {
                    if (
                      await confirm({
                        title: t('confirmDelete', { username: a.username }),
                        confirmLabel: t('delete'),
                        danger: true,
                      })
                    ) {
                      del.mutate(a.username);
                    }
                  }}
                >
                  {t('delete')}
                </Button>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>

      <Dialog open={resetFor !== null} onOpenChange={(o) => !o && setResetFor(null)}>
        <DialogContent className="max-w-sm">
          <DialogHeader>
            <DialogTitle>{t('resetTitle', { username: resetFor ?? '' })}</DialogTitle>
          </DialogHeader>
          <div className="space-y-1">
            <Label htmlFor="reset-pw">{t('newPassword')}</Label>
            <Input
              id="reset-pw"
              type="password"
              autoComplete="new-password"
              value={resetPw}
              onChange={(e) => setResetPw(e.target.value)}
            />
            <p className="text-xs text-muted">{t('passwordHint')}</p>
          </div>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setResetFor(null)}>
              {t('cancel')}
            </Button>
            <Button
              disabled={resetPw.length < 8}
              onClick={() => {
                if (!resetFor) return;
                patch.mutate(
                  { username: resetFor, body: { password: resetPw } },
                  {
                    onSuccess: () => {
                      toast.success(t('toast.passwordReset'));
                      setResetFor(null);
                    },
                  },
                );
              }}
            >
              {t('resetPassword')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
