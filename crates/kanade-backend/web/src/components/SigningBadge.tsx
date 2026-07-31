import { HelpCircle, Shield, ShieldCheck, ShieldOff } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { Badge } from '@/components/ui/badge';
import { keyIds, signingState, type SigningState } from '@/lib/signing';
import type { AgentRow } from '@/lib/types';

// #1253: how each command-signing state looks. Semantic tokens only, via a
// Badge variant — a raw palette class (`bg-emerald-500/15`) would ignore the
// theme, and this cell has to stay readable in both.
//
// Colour never carries the meaning on its own: every state also has an icon
// and a translated label. `enforcing` and `ready` are the pair most likely to
// be told apart by hue alone if it did, and they are also the pair whose
// difference matters most — done versus waiting for a restart.
const LOOK: Record<
  SigningState,
  { variant: 'success' | 'violet' | 'amber' | 'default'; Icon: typeof Shield }
> = {
  // Reports that it refuses what it cannot verify. Green because this is as
  // far as the rollout goes — NOT because the host is proven healthy: nothing
  // here can tell whether its ring matches the key the backend signs with, and
  // a host enforcing on the wrong key refuses everything. See `signingState`.
  enforcing: { variant: 'success', Icon: ShieldCheck },
  // Armed but not firing — the ring is in place and enforcement starts at this
  // host's next agent restart. Most of the fleet lives here during a rollout,
  // so it gets its own colour rather than sharing "unknown"'s grey.
  ready: { variant: 'violet', Icon: Shield },
  // A ring is present; the agent is too old to report enforcement. Grey rather
  // than amber because there is nothing to provision here — the gap is in what
  // this agent can say, not in what it holds.
  keysOnly: { variant: 'default', Icon: Shield },
  // The provisioning queue: holds nothing, and would refuse everything the day
  // enforcement went on. Amber = act on this one.
  none: { variant: 'amber', Icon: ShieldOff },
  // Never reported. Not the same as "no keys" — this host cannot answer at all
  // until it is upgraded, and provisioning it would be the wrong move.
  unknown: { variant: 'default', Icon: HelpCircle },
};

/** One agent's command-signing state (#1165 / #1253).
 *
 *  The tooltip carries what the cell cannot: why this state means what it
 *  does, and which keys are on the ring. `kid:fingerprint` is 33 characters
 *  and there are usually two, so the tooltip lists the ids — the fingerprint
 *  is what makes rings comparable across machines, not what anyone reads at a
 *  glance. */
export function SigningBadge({ agent }: { agent: AgentRow }) {
  const { t } = useTranslation('agents');
  const state = signingState(agent);
  const { variant, Icon } = LOOK[state];
  const ids = keyIds(agent);
  const title = t(`signing.${state}Title`) + (ids.length ? `\n\n${ids.join('\n')}` : '');
  return (
    <Badge variant={variant} title={title}>
      <Icon className="mr-1 size-3" />
      {t(`signing.${state}`)}
    </Badge>
  );
}
