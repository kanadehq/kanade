import { cva, type VariantProps } from 'class-variance-authority';
import type { HTMLAttributes } from 'react';

import { cn } from '@/lib/utils';

// `whitespace-nowrap` so a badge with an icon + label (e.g.
// `<ScrollText/>プローブ` on the Jobs page INVENTORY column) doesn't
// wrap mid-word when the cell is narrow — pill rows on tight columns
// were rendering as two-line pills.
const badgeVariants = cva(
  'inline-flex items-center whitespace-nowrap rounded-full border px-2.5 py-0.5 text-xs font-medium transition-colors',
  {
    variants: {
      variant: {
        default: 'border-transparent bg-muted/15 text-muted',
        success: 'border-transparent bg-success/15 text-success',
        danger: 'border-transparent bg-danger/15 text-danger',
        violet: 'border-transparent bg-violet/15 text-violet',
        amber: 'border-transparent bg-amber/15 text-amber',
      },
    },
    defaultVariants: { variant: 'default' },
  },
);

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement>, VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />;
}
