/**
 * Promise-based confirm dialog — a typed replacement for the
 * `window.confirm(…)` calls scattered across destructive actions
 * (Jobs / Schedules / Activity / Config / Rollout). The standard
 * approach: an app-root `<ConfirmDialogProvider>` owns the modal
 * state, and consumers grab a callable via `useConfirm()`:
 *
 * ```tsx
 * const confirm = useConfirm();
 * if (await confirm({
 *   title: `Delete job ${id}?`,
 *   description: 'Removes from the catalog. Refused if any schedule references it.',
 *   confirmLabel: 'Delete',
 *   danger: true,
 * })) {
 *   del.mutate(id);
 * }
 * ```
 *
 * Wins over `window.confirm`:
 *   * Themed dark UI matching the rest of the SPA chrome (the
 *     browser native dialog renders white-on-the-system regardless).
 *   * Multi-line description support without newline escapes.
 *   * Destructive vs default visual distinction via the `danger`
 *     button variant.
 *   * Focus / event semantics that don't interact awkwardly with
 *     Radix portals (the original window.confirm-inside-DropdownMenu
 *     issue gemini flagged on #105).
 */

import { createContext, useCallback, useContext, useState, type ReactNode } from 'react';

import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';

export interface ConfirmOptions {
  title: string;
  description?: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Stains the confirm button red for delete / revoke / cascade actions. */
  danger?: boolean;
}

type Resolver = (ok: boolean) => void;

interface Pending {
  opts: ConfirmOptions;
  resolve: Resolver;
}

const Ctx = createContext<((opts: ConfirmOptions) => Promise<boolean>) | null>(null);

export function ConfirmDialogProvider({ children }: { children: ReactNode }) {
  const [pending, setPending] = useState<Pending | null>(null);

  const confirm = useCallback((opts: ConfirmOptions) => {
    return new Promise<boolean>((resolve) => {
      // If another confirm is still pending — e.g. a programmatic
      // trigger fired during the modal's exit animation — resolve
      // the previous caller as `false` so its awaiter doesn't hang
      // forever. The new modal replaces it on the next render.
      setPending((prev) => {
        if (prev) prev.resolve(false);
        return { opts, resolve };
      });
    });
  }, []);

  const settle = (ok: boolean) => {
    if (pending) pending.resolve(ok);
    setPending(null);
  };

  return (
    <Ctx.Provider value={confirm}>
      {children}
      <Dialog
        open={pending !== null}
        onOpenChange={(next) => {
          if (!next) settle(false);
        }}
      >
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{pending?.opts.title}</DialogTitle>
            {pending?.opts.description && (
              <DialogDescription className="whitespace-pre-wrap leading-relaxed">
                {pending.opts.description}
              </DialogDescription>
            )}
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="ghost"
              onClick={() => settle(false)}
              // For destructive prompts we want Enter / Space to land
              // on Cancel by default — a stuck key or a quick
              // pre-emptive Enter shouldn't wipe data. Non-danger
              // prompts focus the confirm button as before.
              autoFocus={pending?.opts.danger === true}
            >
              {pending?.opts.cancelLabel ?? 'Cancel'}
            </Button>
            <Button
              variant={pending?.opts.danger ? 'danger' : 'default'}
              onClick={() => settle(true)}
              autoFocus={pending?.opts.danger !== true}
            >
              {pending?.opts.confirmLabel ?? 'Confirm'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </Ctx.Provider>
  );
}

/**
 * Get the promise-style confirm callable. Throws if invoked outside
 * a `<ConfirmDialogProvider>` so a missing provider surfaces at
 * mount time instead of silently no-op'ing the destructive action.
 */
export function useConfirm() {
  const ctx = useContext(Ctx);
  if (ctx === null) {
    throw new Error('useConfirm() must be used inside <ConfirmDialogProvider>');
  }
  return ctx;
}
