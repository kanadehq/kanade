import { describe, expect, test } from 'bun:test';

import { activityUrl } from './activityUrl';

describe('activityUrl', () => {
  test('encodes exact literal matches and includes every execution outcome', () => {
    const ids = { jobId: 'job.+(a)&日本語', execId: 'exec[1]?=x', pcId: 'PC.+[1]' };
    const url = new URL(activityUrl(ids), 'http://localhost');
    for (const [key, value] of [['job_id', ids.jobId], ['exec_id', ids.execId], ['pc_id', ids.pcId]]) {
      const re = new RegExp(url.searchParams.get(key)!);
      expect(re.test(value)).toBe(true);
      expect(re.test(`prefix${value}`)).toBe(false);
      expect(re.test(`${value}suffix`)).toBe(false);
      expect(re.test(value.replace(/[.?]/, 'x'))).toBe(false);
    }
    expect(url.searchParams.has('status')).toBe(false);
  });

  test('keeps Jobs running links and omits unavailable filters', () => {
    const params = new URL(activityUrl({ jobId: 'job', status: 'running' }), 'http://localhost').searchParams;
    expect(Object.fromEntries(params)).toEqual({ status: 'running', job_id: '^job$' });
    expect(activityUrl({ jobId: 'adhoc-run', execId: null, pcId: 'PC' })).toBe(
      '/activity?job_id=%5Eadhoc-run%24&pc_id=%5EPC%24',
    );
  });
});
