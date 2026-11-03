import { useEffect, useState } from 'react';
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
import { locationIsCurrent } from '@/lib/locationFreshness';
import type { DeviceLocationResponse, LocationStatus } from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

function statusBadgeVariant(status: LocationStatus): 'default' | 'success' | 'amber' | 'danger' {
  switch (status) {
    case 'success':
      return 'success';
    case 'offline':
    case 'timeout':
    case 'no_user_session':
    case 'unavailable':
      return 'amber';
    case 'permission_denied':
    case 'location_disabled':
    case 'unsupported':
    case 'expired':
      return 'danger';
    default:
      return 'default';
  }
}

export function AgentLocationCard({ pcId }: { pcId: string }) {
  const { t } = useTranslation('agent-detail');
  const { hasRole, canSee } = useAuth();
  const qc = useQueryClient();
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  const canAccess = hasRole('operator') && canSee('location');

  const locationQ = useQuery({
    queryKey: ['agent-location', pcId],
    queryFn: () => apiFetch<DeviceLocationResponse>(`/api/agents/${encodeURIComponent(pcId)}/location`),
    enabled: !!pcId && canAccess,
    retry: false,
    refetchInterval: 10_000,
  });

  const requestMutation = useMutation({
    mutationFn: () =>
      apiFetch<DeviceLocationResponse>(
        `/api/agents/${encodeURIComponent(pcId)}/location/request`,
        { method: 'POST' },
      ),
    onSuccess: (res) => {
      // The request can complete with a failure (offline, denied, ...).
      if (res.latest_request?.status === 'success') toast.success(t('location.requestSuccess'));
      else toast.error(t(`location.statuses.${res.latest_request?.status ?? 'unavailable'}`));
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
  // A failed refresh or a suspended browser must not keep a cached "current"
  // label alive. Refetch also observes heartbeat expiry and settings changes.
  const current = locationIsCurrent(lastKnown, locationQ.dataUpdatedAt, now, locationQ.isError);

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
          disabled={requestMutation.isPending || data?.enabled === false}
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
                {current ? t('location.currentTitle') : t('location.lastKnownTitle')}
              </h4>
              {data && !data.enabled && (
                <p className="text-sm text-destructive">{t('location.disabled')}</p>
              )}
              {lastKnown && !current && (
                <p className="text-xs text-muted">{t('location.notCurrent')}</p>
              )}
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
                        {t(`location.sources.${lastKnown.source}`)}
                      </div>
                    </div>
                    <div className="col-span-2 md:col-span-4">
                      <div className="text-xs text-muted">{t('location.acquiredAt')}</div>
                      <div>{fmtIsoLocal(lastKnown.acquired_at)}</div>
                    </div>
                  </div>

                  <div className="pt-1">
                    <a
                      href={lastKnown.map_url}
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
