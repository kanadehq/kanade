import { escapeRegExp } from './utils';

/** Activity metadata filters are regexes. Generated links match literal IDs. */
export function activityUrl({ jobId, execId, pcId, status }: {
  jobId?: string;
  execId?: string | null;
  pcId?: string;
  status?: 'running';
}): string {
  const params = new URLSearchParams();
  if (status) params.set('status', status);
  for (const [key, value] of [['job_id', jobId], ['exec_id', execId], ['pc_id', pcId]] as const) {
    if (value) params.set(key, `^${escapeRegExp(value)}$`);
  }
  return `/activity?${params.toString()}`;
}
