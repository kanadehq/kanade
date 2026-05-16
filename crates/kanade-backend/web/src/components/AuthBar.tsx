import * as Dialog from '@radix-ui/react-dialog';
import { LogIn, LogOut } from 'lucide-react';
import { useState } from 'react';

import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useAuth } from '@/lib/auth';

export function AuthBar() {
  const { token, setToken, isAuthenticated } = useAuth();
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState(token);

  return (
    <div className="flex items-center gap-3">
      <span className="text-xs text-muted">
        {isAuthenticated ? 'auth: token set' : 'auth: no token'}
      </span>
      {isAuthenticated ? (
        <Button variant="secondary" size="sm" onClick={() => setToken('')}>
          <LogOut className="size-3.5" />
          logout
        </Button>
      ) : (
        <Dialog.Root open={open} onOpenChange={setOpen}>
          <Dialog.Trigger asChild>
            <Button variant="secondary" size="sm" onClick={() => setDraft(token)}>
              <LogIn className="size-3.5" />
              login
            </Button>
          </Dialog.Trigger>
          <Dialog.Portal>
            <Dialog.Overlay className="fixed inset-0 bg-black/40 backdrop-blur-sm" />
            <Dialog.Content className="fixed left-1/2 top-1/2 w-full max-w-md -translate-x-1/2 -translate-y-1/2 rounded-lg border border-border bg-card p-6 shadow-xl focus:outline-none">
              <Dialog.Title className="text-lg font-bold">Authenticate</Dialog.Title>
              <Dialog.Description className="mt-1 text-sm text-muted">
                Paste the bearer token your backend operator gave you. Stored in localStorage; the
                logout button clears it.
              </Dialog.Description>
              <Input
                type="password"
                autoFocus
                autoComplete="off"
                placeholder="paste token here"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                className="mt-4"
              />
              <div className="mt-5 flex justify-end gap-2">
                <Dialog.Close asChild>
                  <Button variant="secondary" size="sm">cancel</Button>
                </Dialog.Close>
                <Button
                  size="sm"
                  onClick={() => {
                    setToken(draft.trim());
                    setOpen(false);
                  }}
                >
                  save
                </Button>
              </div>
            </Dialog.Content>
          </Dialog.Portal>
        </Dialog.Root>
      )}
    </div>
  );
}
