import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ExternalLink, Loader2, MapPin } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import type { DeviceLocationResponse, LocationStatus } from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

function statusBadgeVariant(status: LocationStatus): 'default' | 'outline' | 'secondary' | 'destructive' {
  switch (status) {
    case 'success':
      return 'default';
    case 'offline':
    case 'timeout':
    case 'no_user_session':
    case 'unavailable':
      return 'secondary';
    case 'permission_denied':
    case 'location_disabled':
    case 'unsupported':
    case 'expired':
      return 'destructive';
    default:
      return 'outline';
  }
}

export function AgentLocationCard({ pcId }: { pcId: string }) {
  const { t } = useTranslation('agent-detail');
  const { hasRole, canSee } = useAuth();
  const qc = useQueryClient();

  const canAccess = hasRole('operator') && canSee('location');

  const locationQ = useQuery({
    queryKey: ['agent-location', pcId],
    queryFn: () => apiFetch<DeviceLocationResponse>(`/api/agents/${encodeURIComponent(pcId)}/location`),
    enabled: !!pcId && canAccess,
    retry: false,
  });

  const requestMutation = useMutation({
    mutationFn: () =>
      apiFetch<{ request_id: string; requested_at: string }>(
        `/api/agents/${encodeURIComponent(pcId)}/location/request`,
        { method: 'POST' },
      ),
    onSuccess: () => {
      toast.success(t('location.requestSuccess'));
      qc.invalidateQueries({ queryKey: ['agent-location', pcId] });
    },
    onError: (err) => {
      toast.error(formatError(err));
    },
  });

  if (!canAccess) {
    return null;
  }

  const data = locationQ.data;
  const lastKnown = data?.last_known;
  const latestReq = data?.latest_request;

  return (
    <Card>
      <CardHeader className="flex flex-row items-start justify-between gap-3">
        <div className="space-y-1">
          <CardTitle className="text-base flex items-center gap-2">
            <MapPin className="size-4 text-primary" />
            <span>{t('location.title')}</span>
          </CardTitle>
          <p className="text-xs text-muted">{t('location.intro')}</p>
        </div>
        <Button
          size="sm"
          disabled={requestMutation.isPending}
          onClick={() => requestMutation.mutate()}
        >
          {requestMutation.isPending ? (
            <Loader2 className="size-4 animate-spin" />
          ) : (
            <MapPin className="size-4" />
          )}
          <span className="ml-2">
            {requestMutation.isPending ? t('location.requesting') : t('location.request')}
          </span>
        </Button>
      </CardHeader>
      <CardContent className="space-y-4">
        {locationQ.isLoading && (
          <div className="flex items-center gap-2 text-muted text-sm py-2">
            <Loader2 className="size-4 animate-spin" />
            {t('loading')}
          </div>
        )}

        {locationQ.error && <ErrorCard title={t('location.title')} error={locationQ.error} />}

        {!locationQ.isLoading && !locationQ.error && (
          <div className="space-y-4">
            {/* Last known location */}
            <div className="space-y-2">
              <h4 className="text-sm font-semibold text-foreground">
                {t('location.lastKnownTitle')}
              </h4>
              {lastKnown ? (
                <div className="space-y-2">
                  <div className="grid grid-cols-2 md:grid-cols-4 gap-3 bg-muted/30 p-3 rounded-md text-sm">
                    <div>
                      <div className="text-xs text-muted">{t('location.latitude')}</div>
                      <div className="font-mono">{lastKnown.latitude.toFixed(6)}</div>
                    </div>
                    <div>
                      <div className="text-xs text-muted">{t('location.longitude')}</div>
                      <div className="font-mono">{lastKnown.longitude.toFixed(6)}</div>
                    </div>
                    <div>
                      <div className="text-xs text-muted">{t('location.accuracy')}</div>
                      <div>{t('location.accuracyMeters', { meters: lastKnown.accuracy_meters.toFixed(1) })}</div>
                    </div>
                    <div>
                      <div className="text-xs text-muted">{t('location.source')}</div>
                      <div>
                        {lastKnown.source === 'gps'
                          ? t('location.sourceGps')
                          : lastKnown.source === 'wifi'
                            ? t('location.sourceWifi')
                            : t('location.sourceCellular')}
                      </div>
                    </div>
                    <div className="col-span-2 md:col-span-4">
                      <div className="text-xs text-muted">{t('location.acquiredAt')}</div>
                      <div>{fmtIsoLocal(lastKnown.acquired_at)}</div>
                    </div>
                  </div>

                  <div className="pt-1">
                    <a
                      href={`https://www.openstreetmap.org/?mlat=${lastKnown.latitude}&mlon=${lastKnown.longitude}#map=16/${lastKnown.latitude}/${lastKnown.longitude}`}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="inline-flex items-center gap-1.5 text-primary hover:underline text-sm font-medium"
                    >
                      <ExternalLink className="size-3.5" />
                      <span>{t('location.openMap')}</span>
                    </a>
                    <p className="text-xs text-muted mt-1">{t('location.mapNotice')}</p>
                  </div>
                </div>
              ) : (
                <p className="text-muted text-sm">{t('location.noLastKnown')}</p>
              )}
            </div>

            <hr className="border-border" />

            {/* Latest request status */}
            <div className="space-y-2">
              <h4 className="text-sm font-semibold text-foreground">
                {t('location.latestRequestTitle')}
              </h4>
              {latestReq ? (
                <div className="flex flex-col sm:flex-row sm:items-center gap-3 text-sm">
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-muted">{t('location.status')}:</span>
                    <Badge variant={statusBadgeVariant(latestReq.status)}>
                      {t(`location.statuses.${latestReq.status}`)}
                    </Badge>
                  </div>
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-muted">{t('location.requestedAt')}:</span>
                    <span>{fmtIsoLocal(latestReq.requested_at)}</span>
                  </div>
                  {latestReq.reason && (
                    <div className="flex items-center gap-2">
                      <span className="text-xs text-muted">{t('location.reason')}:</span>
                      <span className="text-muted">{latestReq.reason}</span>
                    </div>
                  )}
                </div>
              ) : (
                <p className="text-muted text-sm">{t('location.noRequests')}</p>
              )}
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
