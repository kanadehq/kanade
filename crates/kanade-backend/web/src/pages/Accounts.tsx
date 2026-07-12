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
import { FEATURE_NAV_KEY, GATEABLE_FEATURES } from '@/lib/features';

type Account = {
  username: string;
  role: Role;
  disabled: number;
  must_change_pw: number;
  email: string | null;
  /** #1008 page allow-list. `null` = unrestricted (every page). */
  allowed_features: string[] | null;
  created_at: string;
  updated_at: string;
};

type CreateResp = { setup_link_sent: boolean };

const ROLES: Role[] = ['viewer', 'operator', 'admin'];

export function Accounts() {
  const { t } = useTranslation(['accounts', 'common']);
  const { hasRole, username: selfUsername } = useAuth();
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
  const [newEmail, setNewEmail] = useState('');
  const [newRole, setNewRole] = useState<Role>('viewer');
  // reset-password dialog
  const [resetFor, setResetFor] = useState<string | null>(null);
  const [resetPw, setResetPw] = useState('');
  // edit-email dialog
  const [emailFor, setEmailFor] = useState<string | null>(null);
  const [emailVal, setEmailVal] = useState('');
  // page-access dialog (#1008): `restricted=false` ⇒ unrestricted (NULL);
  // `restricted=true` ⇒ only the checked features.
  const [pagesFor, setPagesFor] = useState<string | null>(null);
  const [pagesRestricted, setPagesRestricted] = useState(false);
  const [pagesSet, setPagesSet] = useState<Set<string>>(new Set());

  const openPages = (a: Account) => {
    setPagesFor(a.username);
    // Restricted iff the backend sent an array; anything else (null, or a
    // missing field) is unrestricted.
    setPagesRestricted(Array.isArray(a.allowed_features));
    setPagesSet(new Set(a.allowed_features ?? []));
  };
  const togglePage = (f: string) =>
    setPagesSet((prev) => {
      const next = new Set(prev);
      if (next.has(f)) next.delete(f);
      else next.add(f);
      return next;
    });

  const invalidate = () => qc.invalidateQueries({ queryKey: ['accounts'] });
  const onError = (err: unknown) => toast.error(formatError(err));

  const create = useMutation({
    mutationFn: () =>
      apiFetch<CreateResp>('/api/accounts', {
        method: 'POST',
        body: JSON.stringify({
          username: newUser.trim(),
          // Omit empty password so the backend takes the email-link path.
          password: newPw || undefined,
          role: newRole,
          email: newEmail.trim() || undefined,
        }),
      }),
    onSuccess: (resp) => {
      toast.success(
        resp.setup_link_sent
          ? t('toast.createdLink', { username: newUser.trim() })
          : t('toast.created', { username: newUser.trim() }),
      );
      setNewUser('');
      setNewPw('');
      setNewEmail('');
      setNewRole('viewer');
      invalidate();
    },
    onError,
  });

  // Mail a one-time setup/reset link to the account's stored email.
  const sendLink = useMutation({
    mutationFn: (username: string) =>
      apiFetch(`/api/accounts/${encodeURIComponent(username)}/reset-link`, { method: 'POST' }),
    onSuccess: (_data, username) => toast.success(t('toast.linkSent', { username })),
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

  // For the edit-email dialog: the value currently on file, and whether
  // the input differs from it — used to skip a no-op PATCH on Save.
  const emailOrig = accounts.data?.find((a) => a.username === emailFor)?.email ?? '';
  const emailChanged = emailVal.trim() !== emailOrig;

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
              // Need a username, and either a password (≥8) or an email
              // (which triggers the setup-link path). A non-empty password
              // must still meet the length floor.
              const hasEmail = newEmail.trim().length > 0;
              // Count code points (like the backend's chars().count()), so
              // an emoji password doesn't pass here then 400 on the server.
              const pwLen = [...newPw].length;
              if (!newUser.trim() || (!hasEmail && pwLen < 8) || (newPw && pwLen < 8)) {
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
              <Label htmlFor="new-email">{t('emailOptional')}</Label>
              <Input
                id="new-email"
                type="email"
                autoComplete="off"
                placeholder={t('emailPlaceholder')}
                value={newEmail}
                onChange={(e) => setNewEmail(e.target.value)}
                className="w-52"
              />
            </div>
            <div className="space-y-1">
              <Label htmlFor="new-password">
                {newEmail.trim() ? t('passwordOrLink') : t('password')}
              </Label>
              <Input
                id="new-password"
                type="password"
                autoComplete="new-password"
                placeholder={newEmail.trim() ? t('passwordLinkPlaceholder') : undefined}
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
            <TableHead>{t('email')}</TableHead>
            <TableHead>{t('role')}</TableHead>
            <TableHead>{t('status')}</TableHead>
            <TableHead>{t('pageAccess')}</TableHead>
            <TableHead>{t('created')}</TableHead>
            <TableHead className="text-right">{t('actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {(accounts.data ?? []).map((a) => (
            <TableRow key={a.username}>
              <TableCell label={t('username')} className="font-medium">
                {/* Group the name + badge as one flex item so the card view
                    keeps them together on the value side (not split across
                    the row by justify-content: space-between). */}
                <span className="inline-flex items-center">
                  {a.username}
                  {a.must_change_pw === 1 && (
                    <Badge variant="amber" className="ml-2">
                      {t('mustChange')}
                    </Badge>
                  )}
                </span>
              </TableCell>
              <TableCell label={t('email')} className="text-xs">
                {/* Click to edit the stored email (PATCH /api/accounts). */}
                <button
                  type="button"
                  className="text-left hover:underline"
                  title={t('editEmail')}
                  onClick={() => {
                    setEmailFor(a.username);
                    setEmailVal(a.email ?? '');
                  }}
                >
                  {a.email ? (
                    <span className="break-all">{a.email}</span>
                  ) : (
                    <span className="text-muted">{t('setEmail')}</span>
                  )}
                </button>
              </TableCell>
              <TableCell label={t('role')}>
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
              <TableCell label={t('status')}>
                {a.disabled === 1 ? (
                  <Badge variant="danger">{t('disabled')}</Badge>
                ) : (
                  <Badge variant="success">{t('enabled')}</Badge>
                )}
              </TableCell>
              <TableCell label={t('pageAccess')}>
                {/* Click to edit which pages this account may see (#1008). */}
                <button
                  type="button"
                  className="text-left hover:underline"
                  title={t('editPageAccess')}
                  onClick={() => openPages(a)}
                >
                  {Array.isArray(a.allowed_features) ? (
                    <Badge variant="amber">
                      {t('pageAccessCount', { count: a.allowed_features.length })}
                    </Badge>
                  ) : (
                    <span className="text-muted">{t('pageAccessAll')}</span>
                  )}
                </button>
              </TableCell>
              <TableCell label={t('created')} className="text-muted text-xs">{a.created_at}</TableCell>
              <TableCell className="text-right space-x-2 whitespace-nowrap">
                {/* Mail a setup/reset link; only when the account has an
                    email on file. */}
                {a.email && (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={sendLink.isPending}
                    title={t('sendLinkHint')}
                    onClick={() => sendLink.mutate(a.username)}
                  >
                    {t('sendLink')}
                  </Button>
                )}
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

      <Dialog open={emailFor !== null} onOpenChange={(o) => !o && setEmailFor(null)}>
        <DialogContent className="max-w-sm">
          <DialogHeader>
            <DialogTitle>{t('editEmailTitle', { username: emailFor ?? '' })}</DialogTitle>
          </DialogHeader>
          {/* A <form> gives Enter-to-submit and native email validation
              (type="email" only validates on form submit). Empty is valid
              and clears the address; a malformed non-empty value is blocked
              by the browser before the request. */}
          <form
            className="space-y-4"
            onSubmit={(e) => {
              e.preventDefault();
              // emailChanged also skips a no-op PATCH (and its toast).
              if (!emailFor || !emailChanged) return;
              // Trim → empty string clears the email server-side.
              const next = emailVal.trim();
              patch.mutate(
                { username: emailFor, body: { email: next } },
                {
                  onSuccess: () => {
                    toast.success(
                      next
                        ? t('toast.emailUpdated', { username: emailFor })
                        : t('toast.emailCleared', { username: emailFor }),
                    );
                    setEmailFor(null);
                  },
                },
              );
            }}
          >
            <div className="space-y-1">
              <Label htmlFor="edit-email">{t('email')}</Label>
              <Input
                id="edit-email"
                type="email"
                autoComplete="off"
                placeholder={t('emailPlaceholder')}
                value={emailVal}
                onChange={(e) => setEmailVal(e.target.value)}
              />
              <p className="text-xs text-muted">{t('emailHint')}</p>
            </div>
            <DialogFooter>
              <Button type="button" variant="ghost" onClick={() => setEmailFor(null)}>
                {t('cancel')}
              </Button>
              <Button type="submit" disabled={patch.isPending || !emailChanged}>
                {t('save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      <Dialog open={pagesFor !== null} onOpenChange={(o) => !o && setPagesFor(null)}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('pageAccessTitle', { username: pagesFor ?? '' })}</DialogTitle>
          </DialogHeader>
          <div className="space-y-4">
            <label className="flex items-center gap-2 text-sm font-medium">
              <input
                type="checkbox"
                className="size-4"
                checked={pagesRestricted}
                onChange={(e) => setPagesRestricted(e.target.checked)}
              />
              {t('restrictPages')}
            </label>
            <p className="text-xs text-muted">{t('pageAccessHint')}</p>
            {pagesRestricted && (
              <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 sm:grid-cols-3">
                {GATEABLE_FEATURES.map((f) => {
                  // Guard against self-lockout: an admin editing their own
                  // account can't remove `accounts` (they'd lose the only UI
                  // that could undo it). The service token remains the last
                  // resort, but don't make the footgun a click away.
                  const forced = pagesFor === selfUsername && f === 'accounts';
                  return (
                    <label key={f} className="flex items-center gap-2 text-sm">
                      <input
                        type="checkbox"
                        className="size-4"
                        checked={forced || pagesSet.has(f)}
                        disabled={forced}
                        title={forced ? t('pageAccessSelfLock') : undefined}
                        onChange={() => togglePage(f)}
                      />
                      {t(FEATURE_NAV_KEY[f], { ns: 'common' })}
                    </label>
                  );
                })}
              </div>
            )}
          </div>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setPagesFor(null)}>
              {t('cancel')}
            </Button>
            <Button
              disabled={patch.isPending}
              onClick={() => {
                if (!pagesFor) return;
                // Mirror the UI's self-lockout guard in the payload: an admin
                // restricting their own account keeps `accounts`.
                const isSelf = pagesFor === selfUsername;
                const restrictedSet = isSelf ? new Set([...pagesSet, 'accounts']) : pagesSet;
                patch.mutate(
                  {
                    username: pagesFor,
                    // `null` clears the restriction (unrestricted); an array
                    // (possibly empty = commons only) restricts.
                    body: { allowed_features: pagesRestricted ? [...restrictedSet] : null },
                  },
                  {
                    onSuccess: () => {
                      toast.success(t('toast.pageAccessUpdated', { username: pagesFor }));
                      setPagesFor(null);
                    },
                  },
                );
              }}
            >
              {t('save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
