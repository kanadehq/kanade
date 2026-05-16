import { forwardRef, type LabelHTMLAttributes } from 'react';

import { cn } from '@/lib/utils';

export const Label = forwardRef<HTMLLabelElement, LabelHTMLAttributes<HTMLLabelElement>>(
  ({ className, ...props }, ref) => (
    <label
      ref={ref}
      className={cn('block text-xs font-semibold text-muted mb-1.5 tracking-wide uppercase', className)}
      {...props}
    />
  ),
);
Label.displayName = 'Label';
