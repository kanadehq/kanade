import { useMemo } from 'react';

import { renderNotificationMarkdown } from '@/lib/markdown';
import { cn } from '@/lib/utils';

// Render a notification body (Markdown) as sanitized HTML. Shared by the detail
// view and the compose/edit live previews so an operator sees, before sending,
// exactly what endpoints will render. The HTML is produced by
// `renderNotificationMarkdown`, which sanitizes through a strict DOMPurify
// allowlist — no raw HTML, scripts, or event handlers — so the
// `dangerouslySetInnerHTML` here is safe.
export function NotificationMarkdown({
  body,
  className,
}: {
  body: string;
  className?: string;
}) {
  // The compose/edit forms re-render this on every keystroke in *any* field
  // (title, priority, …); memoize so an unchanged body isn't re-rendered.
  const html = useMemo(() => renderNotificationMarkdown(body), [body]);

  return (
    <div
      className={cn('notif-md text-sm', className)}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
