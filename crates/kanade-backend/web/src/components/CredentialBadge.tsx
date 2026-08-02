import { HelpCircle, KeyRound, ShieldAlert, ShieldCheck, ShieldOff } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { Badge } from '@/components/ui/badge';
import { credentialState, credentialUser, type CredentialState } from '@/lib/credential';
import type { AgentRow } from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

// #1270: how each NATS-credential state looks. Semantic tokens only, via a
// Badge variant — a raw palette class would ignore the theme, and this cell
// has to stay readable in both.
//
// Same rule the signing column set: colour carries URGENCY, the label and icon
// carry the meaning. Two states share grey here and the shared colour is
// honest — neither is work to do — but they are separate badges because their
// next steps differ, and the tooltip says which.
const LOOK: Record<
  CredentialState,
  { variant: 'success' | 'amber' | 'default'; Icon: typeof KeyRound }
> = {
  // Authenticated as a named NATS user: this host is off the shared
  // credential. Green because it is as far as the migration goes.
  named: { variant: 'success', Icon: ShieldCheck },
  // The fleet-wide token. Amber = act on this one: it is the set that has to
  // reach zero before anything can be narrowed server-side.
  shared: { variant: 'amber', Icon: KeyRound },
  // The broker required no credential at all. Amber rather than grey: on a dev
  // broker it is expected, anywhere else it is the loudest thing on the page.
  noAuth: { variant: 'amber', Icon: ShieldOff },
  // On the broker, but the backend cannot prove what it authenticated as.
  // Grey — nothing to provision here; the gap is in the evidence.
  unproven: { variant: 'default', Icon: ShieldAlert },
  // Never correlated. NOT "on the old credential" — this host has said
  // nothing, and provisioning it would be answering a question nobody asked.
  unseen: { variant: 'default', Icon: HelpCircle },
};

/** One agent's NATS credential state (#1270).
 *
 *  What makes this worth a column rather than a field: it is the **broker's**
 *  answer, not the agent's. A host kitted from a stale image reports whatever
 *  it was given; this says what the server actually authenticated it as, and
 *  a host cannot talk its way out of that.
 *
 *  The tooltip carries what the cell cannot — why the state means what it
 *  does, and when it last changed, which is the difference between "moved to
 *  this credential during today's rollout" and "has been like this for
 *  months". */
export function CredentialBadge({ agent }: { agent: AgentRow }) {
  const { t } = useTranslation('agents');
  const state = credentialState(agent);
  const { variant, Icon } = LOOK[state];
  const user = credentialUser(agent);
  // Local time, like every other timestamp in this row (`last_heartbeat`).
  // The raw UTC ISO the API returns is precise and reads as a different kind
  // of value than the cell next to it, which is the point of a tooltip
  // nobody has to translate in their head.
  const since = agent.nats_user_since ? fmtIsoLocal(agent.nats_user_since) : null;
  const title =
    t(`credential.${state}Title`) +
    (user ? `\n\n${user}` : '') +
    (since ? `\n${t('credential.since', { at: since })}` : '');
  return (
    <Badge variant={variant} title={title}>
      <Icon className="mr-1 size-3" />
      {user ?? t(`credential.${state}`)}
    </Badge>
  );
}
