import { Construction } from 'lucide-react';

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export function Placeholder({ name }: { name: string }) {
  return (
    <Card>
      <CardHeader className="items-center text-center">
        <Construction className="size-10 text-amber" />
        <CardTitle>{name} — coming soon</CardTitle>
      </CardHeader>
      <CardContent className="text-center text-muted">
        This page is on the SPA port plan. The endpoint behind it works today (see the legacy
        vanilla SPA at v0.4.0); the React rebuild is incremental.
      </CardContent>
    </Card>
  );
}
