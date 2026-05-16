import { cn } from '@/lib/utils';

/**
 * Pretty-prints a JSON-ish value into a styled <pre>. Used to
 * surface API responses on action pages (Run / Agents / Config /
 * JetStream / …) without inventing one-off components per page.
 */
export function JsonOutput({
  value,
  className,
  empty = '(no output yet)',
}: {
  value: unknown;
  className?: string;
  empty?: string;
}) {
  if (value === undefined || value === null) {
    return <pre className={cn('text-xs text-muted bg-muted/5 p-3 rounded-md', className)}>{empty}</pre>;
  }
  let body: string;
  if (typeof value === 'string') {
    body = value;
  } else {
    try { body = JSON.stringify(value, null, 2); }
    catch { body = String(value); }
  }
  return (
    <pre className={cn('text-xs whitespace-pre-wrap break-words bg-muted/5 p-3 rounded-md max-h-96 overflow-auto', className)}>
      {body}
    </pre>
  );
}
