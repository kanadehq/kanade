/**
 * Shadcn-style Sheet primitive — the same Radix UI Dialog the modal
 * wrapper (`dialog.tsx`) builds on, restyled as a right-edge drawer.
 * Built for master-detail pages (Jobs / Schedules): the list keeps a
 * handful of scannable summary columns and the drawer carries the
 * long-tail fields (full cwd paths, cron internals, …) that used to
 * force the tables into 11–13 columns of truncated cells.
 */

import * as DialogPrimitive from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ElementRef,
  type HTMLAttributes,
} from 'react';

import { cn } from '@/lib/utils';

const Sheet = DialogPrimitive.Root;
const SheetTrigger = DialogPrimitive.Trigger;
const SheetPortal = DialogPrimitive.Portal;
const SheetClose = DialogPrimitive.Close;

const SheetOverlay = forwardRef<
  ElementRef<typeof DialogPrimitive.Overlay>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Overlay>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Overlay
    ref={ref}
    className={cn(
      'fixed inset-0 z-40 bg-black/60',
      'data-[state=open]:animate-overlay-in data-[state=closed]:animate-overlay-out',
      className,
    )}
    {...props}
  />
));
SheetOverlay.displayName = 'SheetOverlay';

const SheetContent = forwardRef<
  ElementRef<typeof DialogPrimitive.Content>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Content>
>(({ children, className, ...props }, ref) => (
  <SheetPortal>
    <SheetOverlay />
    <DialogPrimitive.Content
      ref={ref}
      className={cn(
        // flex-col + h-full so consumers can pin a footer to the
        // bottom with mt-auto while the field list scrolls.
        'fixed inset-y-0 right-0 z-50 flex h-full w-full max-w-lg flex-col gap-4',
        'border-l border-border bg-bg p-6 shadow-lg overflow-y-auto',
        'data-[state=open]:animate-sheet-in data-[state=closed]:animate-sheet-out',
        className,
      )}
      {...props}
    >
      {children}
      <DialogPrimitive.Close
        className="absolute right-4 top-4 rounded-sm text-muted hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-bg"
        aria-label="Close"
      >
        <X className="size-4" />
      </DialogPrimitive.Close>
    </DialogPrimitive.Content>
  </SheetPortal>
));
SheetContent.displayName = 'SheetContent';

function SheetHeader({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn('flex flex-col gap-1.5 pr-8', className)} {...props} />;
}

function SheetFooter({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn('mt-auto flex flex-col-reverse gap-2 sm:flex-row sm:justify-end', className)}
      {...props}
    />
  );
}

const SheetTitle = forwardRef<
  ElementRef<typeof DialogPrimitive.Title>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Title>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Title
    ref={ref}
    className={cn('text-lg font-semibold', className)}
    {...props}
  />
));
SheetTitle.displayName = 'SheetTitle';

const SheetDescription = forwardRef<
  ElementRef<typeof DialogPrimitive.Description>,
  ComponentPropsWithoutRef<typeof DialogPrimitive.Description>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Description
    ref={ref}
    className={cn('text-sm text-muted', className)}
    {...props}
  />
));
SheetDescription.displayName = 'SheetDescription';

export {
  Sheet,
  SheetClose,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetOverlay,
  SheetPortal,
  SheetTitle,
  SheetTrigger,
};
