import type { HTMLAttributes, ReactNode } from 'react';

import { cn } from '@/lib/utils';

/**
 * Key-value field list for detail drawers — a `<dl>` on a two-column
 * grid so every label/value pair lines up without per-row markup.
 * `auto 1fr` keeps the label column as narrow as its longest entry
 * and hands the rest to values, which may hold unbreakable content
 * (Windows paths, cron expressions); `min-w-0` on the `<dd>` lets
 * those wrap/break inside the column instead of widening it.
 */
export function DetailList({ className, ...props }: HTMLAttributes<HTMLDListElement>) {
  return (
    <dl
      className={cn('grid grid-cols-[auto_1fr] items-baseline gap-x-4 gap-y-2 text-sm', className)}
      {...props}
    />
  );
}

export function DetailItem({
  label,
  children,
  className,
}: {
  label: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <>
      <dt className="text-xs uppercase tracking-wide text-muted">{label}</dt>
      <dd className={cn('min-w-0 break-words', className)}>{children}</dd>
    </>
  );
}
