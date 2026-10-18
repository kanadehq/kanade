import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';

import { AgentLocationCard } from '@/components/AgentLocationCard';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useAuth } from '@/lib/auth';

export function Location() {
  const { t } = useTranslation('agent-detail');
  const { hasRole } = useAuth();
  const [params, setParams] = useSearchParams();
  const pcId = params.get('pc_id') ?? '';
  const [draft, setDraft] = useState(pcId);

  if (!hasRole('operator')) return null;

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-bold">{t('location.title')}</h1>
      <form
        className="flex items-end gap-3"
        onSubmit={(event) => {
          event.preventDefault();
          if (draft.trim()) setParams({ pc_id: draft.trim() });
        }}
      >
        <label className="space-y-2" htmlFor="location-pc-id">
          <span className="text-sm">{t('meta.pcId')}</span>
          <Input
            id="location-pc-id"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            required
          />
        </label>
        <Button type="submit" disabled={!draft.trim()}>
          {t('location.showDevice')}
        </Button>
      </form>
      {pcId && <AgentLocationCard key={pcId} pcId={pcId} />}
    </div>
  );
}
