import { Component, type ReactNode } from 'react';
import { RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { isChunkLoadError } from '@/lib/chunkError';

type Props = { children: ReactNode };
type State = { error: Error | null };

/// Catches render errors from the lazily-loaded route chunks (#1215③
/// review). The case that matters is a stale tab after a backend
/// redeploy: the new `index.html` points at new asset hashes, the old
/// tab's next dynamic import 404s, and an uncaught throw during render
/// unmounts the WHOLE app to a blank white screen. With this boundary
/// around `<Outlet/>` the failure is contained to the content area and
/// the operator gets a reload prompt (which pulls the new index.html)
/// instead. Generic render errors get the same containment plus the
/// message and a retry.
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: { componentStack?: string | null }) {
    // The SPA has no telemetry; the console is the only place this
    // lands. Keep it loud — a swallowed boundary failure is worse
    // than a noisy one.
    console.error('[ErrorBoundary]', error, info.componentStack);
  }

  render() {
    if (this.state.error) {
      return (
        <ErrorFallback error={this.state.error} onReset={() => this.setState({ error: null })} />
      );
    }
    return this.props.children;
  }
}

function ErrorFallback({ error, onReset }: { error: Error; onReset: () => void }) {
  const { t } = useTranslation('common');
  const chunk = isChunkLoadError(error);
  return (
    <Card className="border-danger/40">
      <CardHeader>
        <CardTitle className="text-danger">
          {chunk ? t('errorBoundary.updateTitle') : t('errorBoundary.title')}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          {chunk ? t('errorBoundary.updateDescription') : t('errorBoundary.description')}
        </p>
        {!chunk && (
          <pre className="text-sm whitespace-pre-wrap break-words text-danger">{error.message}</pre>
        )}
        <div className="flex gap-2">
          <Button onClick={() => window.location.reload()}>
            <RefreshCw className="size-4" />
            {t('errorBoundary.reload')}
          </Button>
          {!chunk && (
            <Button variant="secondary" onClick={onReset}>
              {t('actions.retry')}
            </Button>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
