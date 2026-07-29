use std::time::Duration;

use anyhow::Result;
use clap::Args;
use futures::StreamExt;
use kanade_shared::signing;
use kanade_shared::wire::{Command, RunAs, Shell};
use kanade_shared::{ExecResult, subject};
use tracing::info;
use uuid::Uuid;

const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Environment pair carrying the break-glass signing key (#1165).
///
/// An environment variable rather than a file or the registry, because the key
/// must **not rest on an operator machine** — that is the rejected "CLI holds a
/// signing key" option. The operator retrieves it from an offline medium during
/// an incident, exports it for that shell, and it goes away with the shell.
///
/// Not perfect: a process environment is readable by the user's own processes,
/// and a careless `export` reaches shell history. What bounds the damage is the
/// key's own policy rather than its storage — a short `max_age` means a leaked
/// key can only sign *fresh* commands, and `audit_every_use` means every use is
/// recorded agent-side, draining through the obs outbox even if the incident
/// took the backend down.
pub const ENV_BREAK_GLASS_KEY: &str = "KANADE_BREAK_GLASS_KEY";
pub const ENV_BREAK_GLASS_KID: &str = "KANADE_BREAK_GLASS_KID";

#[derive(Args, Debug)]
pub struct RunArgs {
    pub pc_id: String,
    #[arg(long, default_value = "powershell")]
    pub shell: String,
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Optional exec_id (formerly `--job-id` pre-v0.29); when set,
    /// `kanade kill <exec_id>` can terminate the run. The CLI still
    /// accepts the historical `--job-id` flag name via clap alias so
    /// existing scripts keep working.
    #[arg(long, alias = "job-id")]
    pub exec_id: Option<String>,
    /// Execution identity: `system` (default), `user`, or
    /// `system_gui` — same values as the manifest's
    /// `execute.run_as`.
    #[arg(long, default_value = "system", value_parser = ["system", "user", "system_gui", "system-gui"])]
    pub run_as: String,
    /// Script body (use `--` before the script to bypass clap flag parsing).
    pub script: Vec<String>,
}

/// Resolve the break-glass signer from the environment.
///
/// `Ok(None)` means unsigned, which is what `kanade run` has always published
/// and what agents still accept — the rollout is at "capability first,
/// enforcement last", so an operator without the key is not blocked today. At
/// stage 3 the same absence becomes a rejection, which is why the log line
/// below says so rather than staying silent about it.
///
/// Half a pair is a hard error. Signing under an invented id would be reported
/// by every provisioned agent as an unattributable signature, and publishing
/// unsigned instead would silently discard the operator's intent to sign
/// mid-incident — neither is a reasonable guess at what they meant.
fn break_glass_signer() -> Result<Option<signing::Signer>> {
    let raw_secret = std::env::var(ENV_BREAK_GLASS_KEY).ok();
    let raw_kid = std::env::var(ENV_BREAK_GLASS_KID).ok();
    // An exported-but-empty variable is "not set" as far as an operator is
    // concerned; treating `KANADE_BREAK_GLASS_KID=` as a present-but-blank id
    // would fail with a confusing unattributable-signature error instead.
    let secret = raw_secret
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let kid = raw_kid.as_deref().map(str::trim).filter(|v| !v.is_empty());
    match signing::pair(secret, kid) {
        Ok(Some((secret, kid))) => Ok(Some(
            signing::Signer::from_secret(secret, kid).map_err(|e| anyhow::anyhow!(e))?,
        )),
        Ok(None) => {
            info!(
                "publishing unsigned — set {ENV_BREAK_GLASS_KEY} and {ENV_BREAK_GLASS_KID} to sign \
                 with the break-glass key. Agents accept unsigned commands today; once command \
                 signing is enforced they will not."
            );
            Ok(None)
        }
        Err(signing::MissingHalf::Kid) => anyhow::bail!(
            "${ENV_BREAK_GLASS_KEY} is set but ${ENV_BREAK_GLASS_KID} is not. The two are only \
             meaningful together — a signature carrying an id no agent holds is rejected as \
             unattributable, so refusing here rather than guessing."
        ),
        Err(signing::MissingHalf::Key) => anyhow::bail!(
            "${ENV_BREAK_GLASS_KID} is set but ${ENV_BREAK_GLASS_KEY} is not. Retrieve the private \
             key from wherever it rests; `kanade command-key break-glass` mints a new one only if \
             the old is genuinely lost, and a new id has to reach every agent before it works."
        ),
    }
}

/// The NATS headers carrying a signature over `payload`.
fn sig_headers(signer: &signing::Signer, payload: &[u8], at_ms: i64) -> async_nats::HeaderMap {
    let h = signer.headers(payload, at_ms);
    let mut map = async_nats::HeaderMap::new();
    for (name, value) in [
        (signing::SIG, h.sig_b64.as_deref()),
        (signing::SIG_KID, h.kid.as_deref()),
        (signing::SIG_ALG, h.alg.as_deref()),
        (signing::SIG_AT, h.at_ms.as_deref()),
    ] {
        if let Some(v) = value {
            map.insert(name, v);
        }
    }
    map
}

pub async fn execute(client: async_nats::Client, args: RunArgs) -> Result<()> {
    if args.script.is_empty() {
        anyhow::bail!("script is empty (did you forget `--`?)");
    }
    let script = args.script.join(" ");
    let request_id = Uuid::new_v4().to_string();
    let shell = match args.shell.as_str() {
        // `ps` stays an alias for Windows PowerShell 5.1. `pwsh` is now
        // its OWN variant (PowerShell 7, cross-platform) — no longer an
        // alias for `powershell` — so a Linux target can be addressed.
        "powershell" | "ps" => Shell::Powershell,
        "cmd" => Shell::Cmd,
        "sh" => Shell::Sh,
        "pwsh" => Shell::Pwsh,
        other => {
            anyhow::bail!("unknown shell {other:?} (use powershell, pwsh, cmd, or sh)")
        }
    };
    // Keep in sync with `kanade_shared::wire::RunAs` (and the
    // `value_parser` list on the arg above) if the enum grows a
    // variant — clap already rejects anything outside that list,
    // so the bail! arm is unreachable today.
    let run_as = match args.run_as.as_str() {
        "system" => RunAs::System,
        "user" => RunAs::User,
        "system_gui" | "system-gui" => RunAs::SystemGui,
        other => anyhow::bail!("unknown run_as {other:?} (use system, user, or system_gui)"),
    };
    let cmd = Command {
        id: "adhoc-run".to_string(),
        version: "0.0.0".to_string(),
        request_id: request_id.clone(),
        exec_id: args.exec_id.clone(),
        shell,
        script,
        // `kanade run` is always inline — there's no Manifest behind
        // it to carry a script_object reference (#210).
        script_object: None,
        script_object_sha256: None,
        timeout_secs: args.timeout,
        jitter_secs: None,
        // Operator-selectable via `--run-as` (defaults to system,
        // the historical behaviour). cwd customisation still
        // belongs on a registered Job + `kanade exec`.
        run_as,
        cwd: None,
        // Ad-hoc inline run; no scheduled tick → no deadline.
        deadline_at: None,
        // v0.26: no Manifest behind this ad-hoc run, so use the
        // back-compat default (`Cached`).
        staleness: kanade_shared::wire::Staleness::Cached,
        // Issue #246: no Manifest → no emit hint. Stdout flows
        // back via ExecResult unchanged.
        emit: None,
        // #290: ad-hoc inline run is never a check.
        check: None,
        // #219: ad-hoc inline run has no manifest → no collect hint.
        collect: None,
        // #418 Phase 4: ad-hoc run has no schedule → no retry policy.
        retry: None,
        // Ad-hoc inline run has no manifest → no finalize hook.
        finalize: None,
    };

    let result_subj = subject::results(&request_id);
    let mut sub = client.subscribe(result_subj.clone()).await?;

    let payload = serde_json::to_vec(&cmd)?;
    let signer = break_glass_signer()?;
    let subject = subject::commands_pc(&args.pc_id);
    match &signer {
        Some(s) => {
            // Signed at publish, so the covered timestamp says when the bytes
            // went out. That matters far more here than for the backend: this
            // key is the one with a freshness bound, and the bound is what it
            // is measured against.
            let headers = sig_headers(s, &payload, chrono::Utc::now().timestamp_millis());
            info!(kid = s.kid(), "signing with the break-glass key");
            client
                .publish_with_headers(subject, headers, payload.into())
                .await?;
        }
        None => {
            client.publish(subject, payload.into()).await?;
        }
    }
    client.flush().await?;
    info!(
        pc_id = %args.pc_id,
        request_id = %request_id,
        exec_id = ?args.exec_id,
        "sent command, waiting for result",
    );

    // Audit at dispatch (not on result): the code ran on the host
    // regardless of whether we hang around for its output. Truncate the
    // script — the audit row should show intent, not be a payload dump.
    crate::audit::record(
        &client,
        "run",
        Some(&args.pc_id),
        serde_json::json!({
            "request_id": request_id,
            "exec_id": args.exec_id,
            "shell": args.shell,
            // The parsed enum (not the raw flag string) so the audit
            // row records the normalized snake_case form even when
            // the operator typed the hyphenated `system-gui` alias.
            "run_as": run_as,
            "script": cmd.script.chars().take(500).collect::<String>(),
        }),
    )
    .await;

    let wait = Duration::from_secs(args.timeout + 10);
    let msg = tokio::time::timeout(wait, sub.next())
        .await
        .map_err(|_| anyhow::anyhow!("timeout waiting for result on {result_subj}"))?
        .ok_or_else(|| anyhow::anyhow!("result subscription closed"))?;
    let result: ExecResult = serde_json::from_slice(&msg.payload)?;

    println!("pc_id     : {}", result.pc_id);
    println!("exit_code : {}", result.exit_code);
    println!("started   : {}", result.started_at);
    println!("finished  : {}", result.finished_at);
    println!("--- stdout ---");
    print!("{}", result.stdout);
    if !result.stdout.ends_with('\n') {
        println!();
    }
    if !result.stderr.is_empty() {
        println!("--- stderr ---");
        print!("{}", result.stderr);
        if !result.stderr.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::signing::{KeyPolicy, KeyRing, SigHeaders, VerifyError, verify};

    /// A signer plus the ring an agent builds from the entry
    /// `kanade command-key break-glass` prints for it. Derived from the signer
    /// rather than from a separately generated key, so the test cannot pass by
    /// accidentally pairing the wrong halves.
    fn signer_and_ring(kid: &str, max_age: Duration) -> (signing::Signer, KeyRing) {
        let key = signing::generate_keypair().unwrap();
        let signer = signing::Signer::from_secret(&signing::encode_secret(&key), kid).unwrap();
        let mut ring = KeyRing::new();
        ring.insert(
            kid,
            signer.verifying_key(),
            KeyPolicy::break_glass("break-glass", max_age),
        );
        (signer, ring)
    }

    fn headers_back(map: &async_nats::HeaderMap) -> SigHeaders {
        let get = |name: &str| map.get(name).map(|v| v.to_string());
        SigHeaders {
            sig_b64: get(signing::SIG),
            kid: get(signing::SIG_KID),
            alg: get(signing::SIG_ALG),
            at_ms: get(signing::SIG_AT),
        }
    }

    #[test]
    fn a_break_glass_run_verifies_against_the_ring_an_agent_holds() {
        // The cross-crate property the recovery path rests on: bytes signed
        // here verify under the entry `kanade command-key break-glass` prints.
        let (signer, ring) = signer_and_ring("break-glass-1", Duration::from_secs(900));

        let body = br#"{"id":"adhoc-run","request_id":"r1"}"#;
        let at = 1_700_000_000_000;
        let map = sig_headers(&signer, body, at);
        let ok = verify(&ring, body, &headers_back(&map), at).expect("verifies");
        assert_eq!(ok.kid, "break-glass-1");
        // Auditing is not optional for this key — the agent logs every use.
        assert!(ok.policy.audit_every_use);
    }

    #[test]
    fn a_captured_break_glass_command_stops_working_once_it_is_stale() {
        // Why the bound exists rather than being belt-and-braces: `kanade run`
        // sets `deadline_at: None`, and the agent's replay dedup is an
        // in-memory cache that is empty on first boot. On a machine that
        // reboots into a JetStream replay, this is the only thing between it and
        // a week-old emergency command.
        let (signer, ring) = signer_and_ring("break-glass-1", Duration::from_secs(900));

        let body = b"emergency";
        let signed_at = 1_700_000_000_000i64;
        let map = sig_headers(&signer, body, signed_at);
        let headers = headers_back(&map);

        // Inside the window.
        assert!(verify(&ring, body, &headers, signed_at + 60_000).is_ok());
        // Past it — genuine, and refused on policy rather than on signature.
        assert!(matches!(
            verify(&ring, body, &headers, signed_at + 3_600_000),
            Err(VerifyError::Stale { .. })
        ));
    }

    #[test]
    fn every_signature_header_travels_and_none_is_blank() {
        // A partial set is classified `Malformed` by the agent, not `Unsigned` —
        // so a missing header turns a legitimate break-glass command into a
        // reported anomaly mid-incident.
        let (signer, _) = signer_and_ring("bg", Duration::from_secs(900));
        let map = sig_headers(&signer, b"body", 1);
        for name in [
            signing::SIG,
            signing::SIG_KID,
            signing::SIG_ALG,
            signing::SIG_AT,
        ] {
            let v = map
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .to_string();
            assert!(!v.is_empty(), "{name} is blank");
        }
    }
}
