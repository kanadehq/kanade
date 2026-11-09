import { PasskeySettingsCard } from './PasskeySettingsCard';
import { usePasskeyEnabled } from '@/lib/usePasskeyEnabled';

export function PasskeySettingsHost({ canOperate = true }: { canOperate?: boolean }) {
  const enabled = usePasskeyEnabled();
  return <>
    <p data-testid="public-probe">{String(enabled)}</p>
    <PasskeySettingsCard canOperate={canOperate} />
  </>;
}
