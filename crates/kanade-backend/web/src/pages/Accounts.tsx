import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { FolderPlus, UserPlus } from 'lucide-react';
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
import { fmtIsoLocal } from '@/lib/utils';

type Account = {
  username: string;
  role: Role;
  disabled: number;
  must_change_pw: number;
  email: string | null;
  /** #1008 page allow-list. `null` = unrestricted (every page). Ignored when
   *  `permission_group` is set (the group governs). */
  allowed_features: string[] | null;
  /** #1008 Phase 3 permission group, or `null`. When set, the group's feature
   *  set is the account's effective access. */
  permission_group: string | null;
  created_at: string;
  updated_at: string;
};

/** #1008 Phase 3 permission group (a reusable, shared page allow-list). */
type PermGroup = {
  name: string;
  features: string[];
  member_count: number;
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
  const groupsQuery = useQuery({
    queryKey: ['permission-groups'],
    queryFn: () => apiFetch<PermGroup[]>('/api/permission-groups'),
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
  // `restricted=true` ⇒ only the checked features. `pagesGroup` (a group name
  // or '') overrides the per-user controls when set (the group governs).
  const [pagesFor, setPagesFor] = useState<string | null>(null);
  const [pagesRestricted, setPagesRestricted] = useState(false);
  const [pagesSet, setPagesSet] = useState<Set<string>>(new Set());
  const [pagesGroup, setPagesGroup] = useState('');
  // group management (#1008 Phase 3)
  const [newGroup, setNewGroup] = useState('');
  const [groupEditFor, setGroupEditFor] = useState<string | null>(null);
  const [groupEditSet, setGroupEditSet] = useState<Set<string>>(new Set());

  const openPages = (a: Account) => {
    setPagesFor(a.username);
    // Restricted iff the backend sent an array; anything else (null, or a
    // missing field) is unrestricted.
    setPagesRestricted(Array.isArray(a.allowed_features));
    setPagesSet(new Set(a.allowed_features ?? []));
    setPagesGroup(a.permission_group ?? '');
  };
  const openGroupEdit = (g: PermGroup) => {
    setGroupEditFor(g.name);
    setGroupEditSet(new Set(g.features));
  };
  const toggleGroupFeature = (f: string) =>
    setGroupEditSet((prev) => {
      const next = new Set(prev);
      if (next.has(f)) next.delete(f);
      else next.add(f);
      return next;
    });
  const togglePage = (f: string) =>
    setPagesSet((prev) => {
      const next = new Set(prev);
      if (next.has(f)) next.delete(f);
      else next.add(f);
      return next;
    });

  // Refresh BOTH lists after any account or group mutation — they're coupled:
  // assigning a user to a group changes that group's `member_count`, and
  // editing/deleting a group changes members' effective access.
  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ['accounts'] });
    qc.invalidateQueries({ queryKey: ['permission-groups'] });
  };
  const onError = (err: unknown) => toast.error(formatError(err));

  const createGroup = useMutation({
    mutationFn: (name: string) =>
      apiFetch('/api/permission-groups', {
        method: 'POST',
        body: JSON.stringify({ name, features: [] }),
      }),
    onSuccess: (_d, name) => {
      toast.success(t('group.toast.created', { name }));
      setNewGroup('');
      invalidate();
    },
    onError,
  });
  const updateGroup = useMutation({
    mutationFn: (v: { name: string; features: string[] }) =>
      apiFetch(`/api/permission-groups/${encodeURIComponent(v.name)}`, {
        method: 'PATCH',
        body: JSON.stringify({ features: v.features }),
      }),
    onSuccess: () => invalidate(),
    onError,
  });
  const deleteGroup = useMutation({
    mutationFn: (name: string) =>
      apiFetch(`/api/permission-groups/${encodeURIComponent(name)}`, { method: 'DELETE' }),
    onSuccess: (_d, name) => {
      toast.success(t('group.toast.deleted', { name }));
      invalidate();
    },
    onError,
  });

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

  // Self-lockout guard for the group path (mirrors the per-user `accounts`
  // guard below): an admin assigning a group to their OWN account must not
  // pick one that omits `accounts`, or they'd lose the only UI that could
  // undo it (the backend enforces the allow-list too — service token /
  // KANADE_AUTH_DISABLE would be the only escape). The per-user path force-
  // keeps `accounts`, but a shared group can't be silently mutated, so here
  // we block the choice instead. `true` for any group lacking `accounts`.
  const groupLocksSelfOut = (name: string) =>
    pagesFor === selfUsername &&
    !(groupsQuery.data ?? []).find((g) => g.name === name)?.features.includes('accounts');
  const selectedGroupLocksSelfOut = !!pagesGroup && groupLocksSelfOut(pagesGroup);

  // No `max-w-5xl` on the container below. The cap is not what garbled the
  // columns — removing it alone changes nothing there — but once the columns
  // have floors it IS what makes the table stick out of its own card: the
  // card stops at 1024px while the table needs 1230px, so the action buttons
  // render 255px outside the border. Uncapped, the card follows the window
  // and the table fits inside it exactly.
  //
  // The two symptoms had two different causes, and reading them as one is
  // why an earlier pass tried removing this, saw the columns still garbled,
  // and put it back.
  return (
    <div className="p-4 md:p-6 space-y-6">
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

      {/* `wideCards`: seven columns plus four action buttons need ~1230px,
          and a 1100px viewport gives this card 786px — as a table it put
          the action column 445px outside its own border. Cards up to
          1535px instead, where every value gets its label and nothing
          overflows. */}
      <Table wideCards resizeKey="accounts" picker>
        <TableHeader>
          <TableRow>
            {/* Floors for the two columns whose content is a single
                unbreakable token. `overflow-wrap: anywhere` (index.css, #1005)
                sets their min-content to ONE character, so `table-layout:
                auto` hands the width to the columns that cannot shrink — the
                role <select>, the action buttons — and starves these to a
                vertical stack of letters. The floor is what stops that; the
                wrap behaviour itself is still wanted for anything longer.

                Trimmed to the smallest values that still hold the columns
                open (measured: 7rem/11rem gives a table min-content of
                1230px against 1262px at 8rem/13rem). It is not free — see
                the PR for what the floors cost in horizontal fit, and what
                the alternatives measured. */}
            <TableHead className="min-w-[7rem]">{t('username')}</TableHead>
            <TableHead className="min-w-[11rem]">{t('email')}</TableHead>
            <TableHead>{t('role')}</TableHead>
            <TableHead>{t('status')}</TableHead>
            <TableHead>{t('pageAccess')}</TableHead>
            <TableHead>{t('created')}</TableHead>
            {/* Left-aligned, like every other actions column in the app
                (Activity, Agents, Schedules, Jobs, Views, Groups). This page
                was the only one right-aligning it. Harmless while the page
                was capped at 5xl and the columns were packed; once the cap
                came off, the actions column collected all the slack and threw
                the buttons to the far edge, leaving a gap between them and
                the timestamp they belong to. */}
            <TableHead>{t('actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {(accounts.data ?? []).map((a) => (
            <TableRow key={a.username}>
              <TableCell label={t('username')} className="font-medium">
                {/* Group the name + badge as one flex item so the card view
                    keeps them together on the value side (not split across
                    the row by justify-content: space-between). */}
                {/* `flex-wrap` + a non-breaking name: with the badge pinned
                    to the same line the cell could not fit both, and
                    `overflow-wrap: anywhere` (index.css) let the browser
                    satisfy the constraint by breaking the USERNAME one
                    character per line instead. Wrapping the badge under the
                    name is the give this cell needs. */}
                <span className="inline-flex flex-wrap items-center gap-y-1">
                  <span className="whitespace-nowrap">{a.username}</span>
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
                    <span>{a.email}</span>
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
                  {a.permission_group ? (
                    // A group governs — show it (distinct violet) rather than
                    // the ignored per-user list.
                    <Badge variant="violet">
                      {t('pageAccessGroup', { name: a.permission_group })}
                    </Badge>
                  ) : Array.isArray(a.allowed_features) ? (
                    <Badge variant="amber">
                      {t('pageAccessCount', { count: a.allowed_features.length })}
                    </Badge>
                  ) : (
                    <span className="text-muted">{t('pageAccessAll')}</span>
                  )}
                </button>
              </TableCell>
              <TableCell
                label={t('created')}
                className="text-muted text-xs whitespace-nowrap"
              >
                {fmtIsoLocal(a.created_at)}
              </TableCell>
              <TableCell className="space-x-2 whitespace-nowrap">
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

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('group.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <p className="text-sm text-muted">{t('group.subtitle')}</p>
          <form
            className="flex flex-wrap items-end gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              const n = newGroup.trim();
              if (n) createGroup.mutate(n);
            }}
          >
            <div className="space-y-1">
              <Label htmlFor="new-group">{t('group.name')}</Label>
              <Input
                id="new-group"
                value={newGroup}
                onChange={(e) => setNewGroup(e.target.value)}
                className="w-52"
              />
            </div>
            <Button type="submit" disabled={createGroup.isPending || !newGroup.trim()}>
              <FolderPlus className="size-4 mr-2" />
              {t('group.create')}
            </Button>
          </form>
          {groupsQuery.isError && (
            <p className="text-red-500 text-sm">{formatError(groupsQuery.error)}</p>
          )}
          {(groupsQuery.data ?? []).length > 0 && (
            <Table resizeKey="accounts.groups" picker>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('group.name')}</TableHead>
                  <TableHead>{t('group.pages')}</TableHead>
                  <TableHead>{t('group.members')}</TableHead>
                  <TableHead>{t('actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(groupsQuery.data ?? []).map((g) => (
                  <TableRow key={g.name}>
                    <TableCell label={t('group.name')} className="font-medium">
                      {g.name}
                    </TableCell>
                    <TableCell label={t('group.pages')}>
                      <button
                        type="button"
                        className="text-left hover:underline"
                        title={t('group.editPages')}
                        onClick={() => openGroupEdit(g)}
                      >
                        {g.features.length > 0 ? (
                          <Badge variant="amber">
                            {t('pageAccessCount', { count: g.features.length })}
                          </Badge>
                        ) : (
                          <span className="text-muted">{t('pageAccessCommonsOnly')}</span>
                        )}
                      </button>
                    </TableCell>
                    <TableCell label={t('group.members')}>{g.member_count}</TableCell>
                    <TableCell className="space-x-2 whitespace-nowrap">
                      <Button variant="secondary" size="sm" onClick={() => openGroupEdit(g)}>
                        {t('group.editPages')}
                      </Button>
                      <Button
                        variant="danger"
                        size="sm"
                        // The backend also refuses (409); disabling here makes
                        // the "reassign members first" rule obvious.
                        disabled={g.member_count > 0}
                        title={g.member_count > 0 ? t('group.deleteBlocked') : undefined}
                        onClick={async () => {
                          if (
                            await confirm({
                              title: t('group.confirmDelete', { name: g.name }),
                              confirmLabel: t('delete'),
                              danger: true,
                            })
                          ) {
                            deleteGroup.mutate(g.name);
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
          )}
        </CardContent>
      </Card>

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
            {/* Group assignment. A group, when chosen, governs the account's
                access (overriding the per-user list below), so those controls
                are disabled while a group is selected. */}
            <div className="space-y-1">
              <Label htmlFor="pages-group">{t('group.assign')}</Label>
              <Select
                id="pages-group"
                value={pagesGroup}
                onChange={(e) => setPagesGroup(e.target.value)}
                className="w-full"
              >
                <option value="">{t('group.none')}</option>
                {(groupsQuery.data ?? []).map((g) => {
                  // Disable groups that would lock the admin out of their own
                  // account (see `groupLocksSelfOut`). A group already assigned
                  // to someone else is still selectable for them.
                  const locksOut = groupLocksSelfOut(g.name);
                  return (
                    <option key={g.name} value={g.name} disabled={locksOut}>
                      {g.name}
                      {locksOut ? ` — ${t('group.selfLockOption')}` : ''}
                    </option>
                  );
                })}
              </Select>
            </div>

            {pagesGroup ? (
              <>
                <p className="text-xs text-muted">
                  {t('group.governedBy', {
                    name: pagesGroup,
                    features:
                      (groupsQuery.data ?? [])
                        .find((g) => g.name === pagesGroup)
                        // A group's stored features are backend keys (a superset
                        // of the SPA's gateable `Feature`), so look the label up
                        // defensively and fall back to the raw key.
                        ?.features.map((f) => {
                          const key = (FEATURE_NAV_KEY as Record<string, string>)[f];
                          return key ? t(key, { ns: 'common' }) : f;
                        })
                        .join(', ') || t('pageAccessCommonsOnly'),
                  })}
                </p>
                {/* A pre-assigned group can survive here even though its option
                    is disabled — warn and block Save so an admin can't leave
                    their own account governed by an Accounts-less group. */}
                {selectedGroupLocksSelfOut && (
                  <p className="text-xs text-red-500">{t('group.selfLockWarn')}</p>
                )}
              </>
            ) : (
              <>
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
              </>
            )}
          </div>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setPagesFor(null)}>
              {t('cancel')}
            </Button>
            <Button
              disabled={patch.isPending || selectedGroupLocksSelfOut}
              onClick={() => {
                if (!pagesFor) return;
                // `permission_group`: a name assigns the group, `null` clears
                // it. The per-user `allowed_features` is only sent when NO group
                // is chosen (otherwise the group governs and we leave the
                // per-user list untouched).
                const body: Record<string, unknown> = { permission_group: pagesGroup || null };
                if (!pagesGroup) {
                  // Mirror the UI's self-lockout guard: an admin restricting
                  // their own account keeps `accounts`.
                  const isSelf = pagesFor === selfUsername;
                  const restrictedSet = isSelf ? new Set([...pagesSet, 'accounts']) : pagesSet;
                  body.allowed_features = pagesRestricted ? [...restrictedSet] : null;
                }
                patch.mutate(
                  { username: pagesFor, body },
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

      <Dialog open={groupEditFor !== null} onOpenChange={(o) => !o && setGroupEditFor(null)}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('group.editTitle', { name: groupEditFor ?? '' })}</DialogTitle>
          </DialogHeader>
          <div className="space-y-3">
            <p className="text-xs text-muted">{t('group.editHint')}</p>
            <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 sm:grid-cols-3">
              {GATEABLE_FEATURES.map((f) => (
                <label key={f} className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    className="size-4"
                    checked={groupEditSet.has(f)}
                    onChange={() => toggleGroupFeature(f)}
                  />
                  {t(FEATURE_NAV_KEY[f], { ns: 'common' })}
                </label>
              ))}
            </div>
          </div>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setGroupEditFor(null)}>
              {t('cancel')}
            </Button>
            <Button
              disabled={updateGroup.isPending}
              onClick={() => {
                if (!groupEditFor) return;
                updateGroup.mutate(
                  { name: groupEditFor, features: [...groupEditSet] },
                  {
                    onSuccess: () => {
                      toast.success(t('group.toast.updated', { name: groupEditFor }));
                      setGroupEditFor(null);
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
