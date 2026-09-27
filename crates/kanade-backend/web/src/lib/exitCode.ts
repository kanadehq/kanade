/**
 * How a finished run reads, shared by every page that badges one.
 *
 * `skipped` is the result's own flag (`ExecResult::skipped` in
 * `crates/kanade-shared/src/wire/result.rs`, persisted as
 * `execution_results.skipped`): the agent published it *instead of* running
 * the script because policy (or the OS) said "not now", so it is neither a
 * success nor a failure. The flag is the only thing that marks a skip; a
 * reserved exit code merely says why. Never read an exit code as a skip: a
 * real script exits 126 / 127 too, and the agent's signature refusal (123) is
 * published unflagged on purpose — both are failures.
 */
export type ExitTone = 'success' | 'skipped' | 'danger';

export function exitTone(code: number, skipped: boolean | undefined): ExitTone {
  if (skipped) return 'skipped';
  return code === 0 ? 'success' : 'danger';
}
