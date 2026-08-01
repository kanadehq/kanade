import { HelpCircle, Shield, ShieldAlert, ShieldCheck, ShieldOff } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { Badge } from '@/components/ui/badge';
import { keyIds, signingState, type BackendSigningKey, type SigningState } from '@/lib/signing';
import type { AgentRow } from '@/lib/types';

// #1253: how each command-signing state looks. Semantic tokens only, via a
// Badge variant — a raw palette class (`bg-emerald-500/15`) would ignore the
// theme, and this cell has to stay readable in both.
//
// Colour carries URGENCY; the label and icon carry the meaning. Two states
// share amber and two share grey, and in both pairs the shared colour is the
// honest signal: same urgency, different reason, and the reason is written
// out rather than encoded in a hue nobody can decode.
const LOOK: Record<
  SigningState,
  { variant: 'success' | 'violet' | 'amber' | 'danger' | 'default'; Icon: typeof Shield }
> = {
  // Reports that it refuses what it cannot verify, AND (when the backend key
  // is known) holds that exact key. Green because this is as far as the
  // rollout goes.
  enforcing: { variant: 'success', Icon: ShieldCheck },
  // Armed but not firing — the ring is in place and enforcement starts at this
  // host's next agent restart. Most of the fleet lives here during a rollout,
  // so it gets its own colour rather than sharing "unknown"'s grey.
  ready: { variant: 'violet', Icon: Shield },
  // A ring is present; the agent is too old to report enforcement. Grey rather
  // than amber because there is nothing to provision here — the gap is in what
  // this agent can say, not in what it holds.
  keysOnly: { variant: 'default', Icon: Shield },
  // #1229's headline case, and the only DANGER state: this host holds the
  // backend's current kid with different bytes. It refuses every command once
  // enforcement is on and never self-heals. Red because it is not merely
  // un-provisioned — either a provisioning run went wrong, or something wrote
  // to the trust root.
  wrongKey: { variant: 'danger', Icon: ShieldAlert },
  // Holds a ring, but not the key the backend signs with today. Amber, not
  // red: this is the ordinary state of a machine a rotation has not reached
  // yet, and the fix is the same provisioning job.
  staleRing: { variant: 'amber', Icon: ShieldAlert },
  // The provisioning queue: holds nothing, and would refuse everything the day
  // enforcement went on. Amber = act on this one.
  none: { variant: 'amber', Icon: ShieldOff },
  // Never reported. Not the same as "no keys" — this host cannot answer at all
  // until it is upgraded, and provisioning it would be the wrong move.
  unknown: { variant: 'default', Icon: HelpCircle },
};

/** One agent's command-signing state (#1165 / #1253 / #1260).
 *
 *  `backend` is what turns this from a report into a check: without it the
 *  badge can only repeat what the agent said, and a host holding the right id
 *  with the wrong key is indistinguishable from a correct one. Optional so the
 *  column still renders when the backend is not signing — abstaining is the
 *  honest answer there, not a verdict.
 *
 *  The tooltip carries what the cell cannot: why this state means what it
 *  does, and which keys are on the ring. `kid:fingerprint` is 33 characters
 *  and there are usually two, so the tooltip lists the ids. */
export function SigningBadge({
  agent,
  backend,
}: {
  agent: AgentRow;
  backend?: BackendSigningKey;
}) {
  const { t } = useTranslation('agents');
  const state = signingState(agent, backend);
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
