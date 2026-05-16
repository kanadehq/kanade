import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';

/**
 * shadcn-style class merger: clsx for conditional joins,
 * tailwind-merge to dedupe conflicting Tailwind atoms (e.g.
 * `cn("p-2", "p-4")` keeps only `p-4`).
 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
