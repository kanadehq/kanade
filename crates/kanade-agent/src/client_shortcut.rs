//! Operator-configured all-users Start-Menu shortcut for the
//! end-user Client App.
//!
//! The shortcut backs two user-visible surfaces:
//!   1. the **Start-Menu label** (the `.lnk` file name, minus
//!      extension), and
//!   2. the **toast sender name** — Windows renders a non-MSIX desktop
//!      app's WinRT toasts under the display name of the Start-Menu
//!      shortcut whose `System.AppUserModel.ID` matches the AUMID the
//!      app tags its toasts with (`com.yukimemi.kanade-client`).
//!
//! Both must read as the operator's per-customer product name (e.g.
//! `端末管理支援ツール`) rather than the internal `kanade`. That name
//! lives in `agent_config.client_display_name`, so the agent — which
//! already resolves the layered config onto a watch channel and runs
//! as LocalSystem (it can write the all-users Start-Menu under
//! `%ProgramData%`) — owns the shortcut: it (re)creates it at startup
//! and re-stamps it whenever the configured name changes, so the
//! Start-Menu label + toast sender follow the backend config with no
//! reinstall.
//!
//! Cost: the watch wait is event-driven (no polling), and the actual
//! PowerShell re-stamp only runs when the resolved name *changes* — an
//! in-memory guard plus a `<data_dir>/client-shortcut.json` state file
//! make unrelated config ticks (heartbeat cadence, rollout target, …)
//! no-ops. In steady state that means zero shortcut work.
//!
//! Windows-only: the Client App, the Start-Menu, and WinRT toasts are
//! all Windows concepts. The module is `#[cfg]`-gated at the agent
//! entrypoint so non-Windows builds don't compile it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use kanade_shared::wire::EffectiveConfig;

/// Built-in fallback product name when no scope sets
/// `client_display_name`. Aliases the single Rust source of truth in
/// `kanade-shared` so this and the client's `app.rs` default can't drift;
/// the WebView (`main.ts` / `index.html`) mirrors the same literal.
const DEFAULT_DISPLAY_NAME: &str = kanade_shared::DEFAULT_CLIENT_DISPLAY_NAME;

/// AUMID the client pins and tags its toasts with — must match
/// `kanade_client::app::APP_USER_MODEL_ID` and the tauri `identifier`.
/// Stamped onto the shortcut so Windows maps the toast back to it.
const AUMID: &str = "com.yukimemi.kanade-client";

/// Legacy fixed-name shortcut earlier shipped by
/// `install-kanade-client.ps1`; removed on first sync so the
/// Start-Menu doesn't show both it and the configured name.
const LEGACY_LNK_NAME: &str = "Kanade Client.lnk";

/// Persisted record of the shortcut the agent last materialised, so a
/// rename can delete the previous `.lnk` and a reboot can tell
/// "already done" from "needs (re)creating" without a PowerShell spawn.
#[derive(Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
struct ShortcutState {
    display_name: String,
    lnk_path: String,
}

/// Spawn the shortcut-sync task. Subscribes to the effective-config
/// watch channel and keeps the all-users Start-Menu shortcut in step
/// with `client_display_name`.
pub fn spawn(cfg_rx: watch::Receiver<EffectiveConfig>) {
    tokio::spawn(sync_loop(cfg_rx));
}

/// Resolve the desired display name from the effective config, falling
/// back to the built-in default. Trims + treats blank as unset (same
/// posture as the handshake handler) so a backend that "clears" by
/// writing `""` doesn't produce a `.lnk` named after whitespace.
fn desired_name(cfg: &EffectiveConfig) -> String {
    cfg.client_display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_DISPLAY_NAME.to_string())
}

/// How often to re-attempt while the shortcut still isn't materialised
/// (client exe not yet installed, or a transient failure). Only ticks
/// in that pending window — once the shortcut exists the loop is purely
/// event-driven (zero steady-state cost). Bounds the "client installed
/// after the agent booted, but the configured name never changed" heal
/// time: `config_supervisor` only republishes on a KV *value* change,
/// so without this a stable-name fleet would never re-check and the
/// shortcut (hence WinRT toast rendering) would never appear until the
/// next agent restart (CodeRabbit PR #670).
const PENDING_RETRY_INTERVAL: Duration = Duration::from_secs(300);

async fn sync_loop(mut rx: watch::Receiver<EffectiveConfig>) {
    // In-memory guard so unrelated config changes (heartbeat cadence,
    // rollout target, …) don't even touch the filesystem once the name
    // is settled. `None` until the first apply, so we always run once
    // at startup.
    let mut last_applied: Option<String> = None;
    loop {
        // Whether the shortcut still needs (re)creating — set on every
        // iteration below (true on a NoClient/error outcome), then read
        // by the wait at the bottom to decide event-driven vs timed retry.
        let pending;
        let desired = desired_name(&rx.borrow());
        if last_applied.as_deref() == Some(desired.as_str()) {
            pending = false;
        } else {
            match ensure_shortcut(&desired).await {
                Ok(SyncOutcome::Synced) => {
                    info!(display_name = %desired, "client Start-Menu shortcut synced");
                    last_applied = Some(desired);
                    pending = false;
                }
                Ok(SyncOutcome::AlreadyCurrent) => {
                    // World already matched — record it so unrelated
                    // config ticks stop re-checking the filesystem.
                    last_applied = Some(desired);
                    pending = false;
                }
                Ok(SyncOutcome::NoClient) => {
                    // Expected on a backend/agent-only host (no client
                    // installed). Leave last_applied unset and stay
                    // `pending` so the timed retry re-checks once the
                    // client exe lands — at debug, not warn, so a
                    // co-located backend box doesn't log a scary line.
                    debug!(display_name = %desired, "client exe absent — shortcut sync skipped");
                    pending = true;
                }
                Err(e) => {
                    // Don't record as applied — the timed retry will try
                    // again. A failure here only affects the Start-Menu
                    // label / toast sender name, never the agent itself.
                    warn!(error = %e, display_name = %desired, "client shortcut sync failed");
                    pending = true;
                }
            }
        }

        // Wait for the next config change. While `pending` (shortcut not
        // yet materialised), also wake on a timeout so the loop re-checks
        // external state (exe install) even when no KV value changed —
        // without turning into a permanent poll once it's settled.
        if pending {
            match tokio::time::timeout(PENDING_RETRY_INTERVAL, rx.changed()).await {
                Err(_elapsed) => continue, // retry tick
                Ok(Ok(())) => continue,    // config changed
                Ok(Err(_)) => return,      // sender dropped — shutting down
            }
        } else if rx.changed().await.is_err() {
            // Sender dropped — agent is shutting down.
            return;
        }
    }
}

/// What [`ensure_shortcut`] did, so the caller knows whether to record
/// the name as applied (and stop re-checking) or keep retrying.
enum SyncOutcome {
    /// (Re)wrote the shortcut via PowerShell.
    Synced,
    /// Nothing to do — state matched, the `.lnk` exists, no legacy
    /// leftover. No PowerShell spawned.
    AlreadyCurrent,
    /// The client exe isn't installed on this host, so no shortcut was
    /// created (not an error — a co-located backend/agent box never
    /// gets one). The caller retries on a later config change.
    NoClient,
}

/// Create/refresh the shortcut for `display_name`.
async fn ensure_shortcut(display_name: &str) -> std::io::Result<SyncOutcome> {
    let programs = start_menu_programs_dir();
    // The `.lnk` *file name* (= the Start-Menu label) must be a legal
    // Windows filename; an operator-set name can contain reserved
    // characters (`\ / : * ? " < > |`). Sanitise the filename stem so
    // `format!` can't produce an invalid/unexpected path. The window
    // title / header / shortcut Description keep the raw name — only the
    // on-disk filename is constrained.
    let desired_lnk = programs.join(format!("{}.lnk", sanitize_lnk_stem(display_name)));
    let legacy_lnk = programs.join(LEGACY_LNK_NAME);
    let state_path = state_file_path();
    let prev = read_state(&state_path);

    // Skip the spawn entirely when the world already matches: same
    // name, the target `.lnk` exists, and no legacy leftover lingers.
    let already_current = prev
        .as_ref()
        .is_some_and(|s| s.display_name == display_name && s.lnk_path == path_str(&desired_lnk))
        && desired_lnk.exists()
        && !legacy_lnk.exists();
    if already_current {
        return Ok(SyncOutcome::AlreadyCurrent);
    }

    // Only materialise the shortcut where the client is actually
    // installed — avoids a dead Start-Menu entry (and an icon-less
    // toast mapping) on a backend/agent-only host. On real endpoints
    // the client exe is present; a client installed after the agent
    // booted is picked up on the next config change or agent restart
    // (routine via self-update).
    let exe = client_exe_path();
    if !exe.exists() {
        return Ok(SyncOutcome::NoClient);
    }

    let prev_lnk = prev
        .as_ref()
        .map(|s| s.lnk_path.clone())
        .unwrap_or_default();
    run_shortcut_script(display_name, &exe, &desired_lnk, &prev_lnk, &legacy_lnk).await?;

    write_state(
        &state_path,
        &ShortcutState {
            display_name: display_name.to_string(),
            lnk_path: path_str(&desired_lnk),
        },
    );
    Ok(SyncOutcome::Synced)
}

/// All-users Start-Menu Programs dir (`%ProgramData%\Microsoft\Windows\
/// Start Menu\Programs`).
fn start_menu_programs_dir() -> PathBuf {
    let program_data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    program_data.join(r"Microsoft\Windows\Start Menu\Programs")
}

/// Installed client binary path (`%ProgramFiles%\Kanade\kanade-client.exe`)
/// — matches `install-kanade-client.ps1`'s `$InstallDir`.
fn client_exe_path() -> PathBuf {
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    program_files.join(r"Kanade\kanade-client.exe")
}

fn state_file_path() -> PathBuf {
    kanade_shared::default_paths::data_dir().join("client-shortcut.json")
}

/// Make `name` safe to use as a Windows `.lnk` filename stem: replace
/// the reserved characters (`< > : " / \ | ? *`) and control chars with
/// `_`, then strip trailing dots/spaces (Windows silently drops those
/// from filenames, which would desync the on-disk name from the state
/// record). Falls back to the default product name if the result is
/// empty (e.g. a name that was *only* reserved characters).
fn sanitize_lnk_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_end_matches(['.', ' ']).trim();
    if trimmed.is_empty() {
        return DEFAULT_DISPLAY_NAME.to_string();
    }
    // Reserved Windows device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9)
    // can't be filenames even with an extension — Windows checks the base
    // name before the first dot, case-insensitively. Suffix `_` so e.g. a
    // product literally named "CON" still produces a creatable `.lnk`.
    if is_reserved_device_name(trimmed) {
        return format!("{trimmed}_");
    }
    trimmed.to_string()
}

/// True if `stem` (case-insensitively, taking the part before the first
/// `.`) is a reserved Windows device name.
fn is_reserved_device_name(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or(stem).to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base.len() == 4
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn read_state(path: &Path) -> Option<ShortcutState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_state(path: &Path, state: &ShortcutState) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_vec_pretty(state) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(path, bytes) {
                warn!(error = %e, path = %path.display(), "write client-shortcut state failed");
            }
        }
        Err(e) => warn!(error = %e, "encode client-shortcut state failed"),
    }
}

/// Run the PowerShell that deletes stale shortcuts and creates the
/// desired one with the AUMID stamped. Dynamic values are passed via
/// environment variables (never interpolated into the script body) so
/// an arbitrary display name can't break or inject into the script.
async fn run_shortcut_script(
    display_name: &str,
    exe: &Path,
    desired_lnk: &Path,
    prev_lnk: &str,
    legacy_lnk: &Path,
) -> std::io::Result<()> {
    use tokio::process::Command;

    // Per-process filename so two agents (or a leftover from a crashed
    // run) can't race on the same temp path. The script body is static
    // (all dynamic values arrive via env), so the content is constant.
    let script_path =
        std::env::temp_dir().join(format!("kanade-client-shortcut-{}.ps1", std::process::id()));
    std::fs::write(&script_path, SHORTCUT_PS1)?;

    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(&script_path)
    .env("KSC_DISPLAY_NAME", display_name)
    .env("KSC_EXE_PATH", exe)
    .env("KSC_AUMID", AUMID)
    .env("KSC_LNK_PATH", desired_lnk)
    .env("KSC_PREV_LNK", prev_lnk)
    .env("KSC_LEGACY_LNK", legacy_lnk)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    // CREATE_NO_WINDOW: the agent runs in session 0 (no console), but
    // belt-and-braces against a stray flash if ever run interactively.
    // `creation_flags` is tokio::process::Command's own Windows-only
    // inherent method (no std CommandExt import needed); this whole
    // module is `#[cfg(windows)]`-gated at the entrypoint.
    cmd.creation_flags(0x0800_0000);

    // Run, then ALWAYS remove the temp script — capture the result first
    // so a spawn/IO error doesn't leak the file via an early `?`.
    let result = cmd.output().await;
    let _ = std::fs::remove_file(&script_path);
    let out = result?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(std::io::Error::other(format!(
            "shortcut script exited {}: {}",
            out.status,
            stderr.trim()
        )));
    }
    Ok(())
}

/// The shortcut script. Reads all dynamic values from `KSC_*` env vars
/// (set by [`run_shortcut_script`]) so nothing operator-controlled is
/// spliced into the source. The IPropertyStore AUMID-stamping interop
/// mirrors `configs/jobs/installers/scripts/install-kanade-client.ps1`
/// (which no longer creates the shortcut — the agent owns it now).
const SHORTCUT_PS1: &str = r#"
$ErrorActionPreference = 'Stop'

$display = $env:KSC_DISPLAY_NAME
$exe     = $env:KSC_EXE_PATH
$aumid   = $env:KSC_AUMID
$lnkPath = $env:KSC_LNK_PATH

# Remove stale shortcuts (the previous custom name + the legacy fixed
# name) so the Start-Menu shows exactly one entry under the current
# name. Never delete the target we're about to (re)create.
# -LiteralPath: the path embeds the operator's display name, which can
# contain PowerShell wildcard-class chars (`[ ] * ?`); without it
# Test-Path/Remove-Item would glob and could match/delete the wrong file.
foreach ($stale in @($env:KSC_PREV_LNK, $env:KSC_LEGACY_LNK)) {
  if ($stale -and ($stale -ne $lnkPath) -and (Test-Path -LiteralPath $stale)) {
    Remove-Item -LiteralPath $stale -Force -ErrorAction SilentlyContinue
  }
}

# 1) Basic shortcut (target/description) via WScript.Shell. The icon is
#    the exe's own embedded icon (the baton mark), so no IconLocation.
$ws = New-Object -ComObject WScript.Shell
$lnk = $ws.CreateShortcut($lnkPath)
$lnk.TargetPath = $exe
$lnk.Description = $display
$lnk.Save()

# 2) Stamp System.AppUserModel.ID via IPropertyStore — the COM interop
#    WScript.Shell can't reach. Without this the AUMID->shortcut mapping
#    is missing and WinRT toasts silently don't render.
if (-not ([System.Management.Automation.PSTypeName]'Kanade.ShortcutAumid').Type) {
  Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace Kanade {
  [StructLayout(LayoutKind.Sequential, Pack = 4)]
  public struct PropertyKey { public Guid fmtid; public uint pid; }

  [StructLayout(LayoutKind.Explicit, Size = 16)]
  public struct PropVariant {
    [FieldOffset(0)] public ushort vt;
    [FieldOffset(8)] public IntPtr p;
  }

  [ComImport, Guid("886d8eeb-8cf2-4446-8d02-cdba1dbdcf99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  public interface IPropertyStore {
    int GetCount(out uint c);
    int GetAt(uint i, out PropertyKey k);
    int GetValue(ref PropertyKey k, out PropVariant v);
    int SetValue(ref PropertyKey k, ref PropVariant v);
    int Commit();
  }

  [ComImport, Guid("0000010b-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  public interface IPersistFile {
    int GetClassID(out Guid id);
    int IsDirty();
    int Load([MarshalAs(UnmanagedType.LPWStr)] string f, int mode);
    int Save([MarshalAs(UnmanagedType.LPWStr)] string f, [MarshalAs(UnmanagedType.Bool)] bool remember);
    int SaveCompleted([MarshalAs(UnmanagedType.LPWStr)] string f);
    int GetCurFile([MarshalAs(UnmanagedType.LPWStr)] out string f);
  }

  public static class ShortcutAumid {
    [DllImport("ole32.dll")] static extern int PropVariantClear(ref PropVariant pvar);

    public static void Set(string lnk, string aumid) {
      Type slType = Type.GetTypeFromCLSID(new Guid("00021401-0000-0000-C000-000000000046"));
      object sl = Activator.CreateInstance(slType);
      PropVariant pv = new PropVariant();
      try {
        ((IPersistFile)sl).Load(lnk, 2); // STGM_READWRITE so Save() works
        IPropertyStore ps = (IPropertyStore)sl;
        // PKEY_AppUserModel_ID = {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5
        PropertyKey key = new PropertyKey {
          fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), pid = 5
        };
        pv.vt = 31 /*VT_LPWSTR*/;
        pv.p = Marshal.StringToCoTaskMemUni(aumid);
        ps.SetValue(ref key, ref pv);
        ps.Commit();
        ((IPersistFile)sl).Save(lnk, true);
      } finally {
        PropVariantClear(ref pv);
        Marshal.ReleaseComObject(sl);
      }
    }
  }
}
'@
}
[Kanade.ShortcutAumid]::Set($lnkPath, $aumid)
Write-Output 'ok'
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(name: Option<&str>) -> EffectiveConfig {
        EffectiveConfig {
            client_display_name: name.map(str::to_owned),
            ..EffectiveConfig::builtin_defaults()
        }
    }

    #[test]
    fn desired_name_falls_back_to_default_when_unset() {
        assert_eq!(desired_name(&cfg_with(None)), DEFAULT_DISPLAY_NAME);
    }

    #[test]
    fn desired_name_uses_configured_value() {
        assert_eq!(
            desired_name(&cfg_with(Some("社内端末ツール"))),
            "社内端末ツール"
        );
    }

    #[test]
    fn desired_name_treats_blank_as_unset() {
        assert_eq!(desired_name(&cfg_with(Some("   "))), DEFAULT_DISPLAY_NAME);
    }

    #[test]
    fn sanitize_lnk_stem_replaces_reserved_chars() {
        assert_eq!(
            sanitize_lnk_stem(r#"a/b\c:d*e?f"g<h>i|j"#),
            "a_b_c_d_e_f_g_h_i_j"
        );
        // A normal Japanese name passes through untouched.
        assert_eq!(
            sanitize_lnk_stem("端末管理支援ツール"),
            "端末管理支援ツール"
        );
    }

    #[test]
    fn sanitize_lnk_stem_trims_trailing_dots_and_spaces() {
        // Windows silently drops trailing dots/spaces from filenames;
        // trimming keeps the on-disk name in sync with the state record.
        assert_eq!(sanitize_lnk_stem("ツール .. "), "ツール");
    }

    #[test]
    fn sanitize_lnk_stem_suffixes_reserved_device_names() {
        // Reserved names (any case, with or without a trailing dotted
        // part) get a `_` suffix so they're creatable as filenames.
        assert_eq!(sanitize_lnk_stem("CON"), "CON_");
        assert_eq!(sanitize_lnk_stem("nul"), "nul_");
        assert_eq!(sanitize_lnk_stem("Com1"), "Com1_");
        assert_eq!(sanitize_lnk_stem("LPT9"), "LPT9_");
        assert_eq!(sanitize_lnk_stem("CON.foo"), "CON.foo_");
        // Non-reserved lookalikes pass through untouched.
        assert_eq!(sanitize_lnk_stem("CONSOLE"), "CONSOLE");
        assert_eq!(sanitize_lnk_stem("COM10"), "COM10");
        assert_eq!(sanitize_lnk_stem("COM0"), "COM0");
    }

    #[test]
    fn sanitize_lnk_stem_empty_result_falls_back_to_default() {
        // A name made entirely of reserved chars sanitises to all
        // underscores (non-empty), but one that's only dots/spaces trims
        // to empty → default.
        assert_eq!(sanitize_lnk_stem("  ...  "), DEFAULT_DISPLAY_NAME);
    }

    #[test]
    fn shortcut_state_round_trips() {
        let s = ShortcutState {
            display_name: "端末管理支援ツール".into(),
            lnk_path: r"C:\ProgramData\...\端末管理支援ツール.lnk".into(),
        };
        let json = serde_json::to_vec(&s).unwrap();
        let back: ShortcutState = serde_json::from_slice(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn default_name_matches_client_default() {
        // Tripwire on the shared literal (this + app.rs alias it from
        // kanade-shared, so they can't drift; the WebView main.ts /
        // index.html mirror the same string and must match this value).
        assert_eq!(DEFAULT_DISPLAY_NAME, "端末管理支援ツール");
    }
}
