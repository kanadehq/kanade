import { CircleAlert } from 'lucide-react';

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export function ErrorCard({ title, error }: { title: string; error: Error | unknown }) {
  const message = error instanceof Error ? error.message : String(error);
  return (
    <Card className="border-danger/40">
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-danger">
          <CircleAlert className="size-5" />
          {title}
        </CardTitle>
      </CardHeader>
      <CardContent>
        <pre className="text-sm whitespace-pre-wrap break-words text-danger">{message}</pre>
      </CardContent>
    </Card>
  );
}
