# Client policies and adapter boundaries

Stage 1 of SPEC §2.11.7 provides host-independent library policies. It does
not enable a Linux/macOS GUI: the non-Windows executable still exits as a
shim. No session discovery, private Unix storage implementation, launch helper,
notification implementation, installer or updater is provided here.

- `paths::ClientPaths::resolve` takes an explicit account UID/home and target-user
  XDG values. Linux accepts only Unix absolute nonempty overrides; unset, empty,
  relative and Windows drive paths fall back to `.config`, `.local/state` and
  `.cache` under that home. macOS uses Library/Application Support, Library/Logs
  and Library/Caches as specified. UI size is persistent state, outside cache.
  `UserPathsResolver` must obtain account data and environment for that UID;
  inherited root HOME/XDG values are not valid inputs. Paths retain OsString
  data and do not require UTF-8.
- Paths alone do not enforce user isolation. `PrivateUserStorage` is the later
  OS boundary for ownership/no-symlink checks and creation as the target UID
  with directories 0700 and files 0600. `private_storage_access` fails closed
  on foreign ownership, identity, permissions or incomplete checks. Facts must
  cover all private components, not just the final file. Shared absolute XDG
  overrides cannot grant another UID access. Race-safe filesystem operations
  remain the responsibility of that unimplemented adapter; callers must not
  use legacy `window_size::save` directly on unverified Unix paths.
- `startup::StartupIntent` separates initial visibility from warm activation.
  Manual launches and deep links show the UI; tray, emergency and resurface
  launches stay hidden. A repeated tray launch leaves existing visibility unchanged. Mixed arguments
  retain the existing Windows precedence: cold emergency/resurface overrides
  focus; warm resurface precedes focus, then emergency. Windows uses the shared
  decision with tray=false to preserve its existing argument handling. Unix
  argv and URL-open adapters must supply tray/deep-link facts later. URI parsers
  retain Windows compatibility; the proposed stricter Unix URI parser is not
  implemented by this stage.
- `session::automatic_delivery` requires matching UID/session, an explicitly
  supported environment, current evidence, a unique selected local active GUI,
  and verified unlock. Unknown, stale, locked, ambiguous, SSH/headless, remote,
  unsupported and mismatched sessions use history only. `GuiSessionResolver`
  must corroborate OS/helper evidence and recheck immediately before delivery;
  no freshness duration or discovery API is invented here. Only Apple Silicon
  macOS 14+ and Ubuntu 24.04 x86_64 GNOME X11/Wayland are initial environments.
  This policy has no authority over KLP admission, connection ownership,
  fan-out, history retrieval or acknowledgement. Manual history remains usable.
- `window_size` is the existing shared implementation: logical size validation,
  minimum/work-area clamp, corrupt-state fallback and atomic save.
  `save_best_effort` shares the existing warn-and-continue GUI save behavior.
  New Unix state directories/temp files are created with 0700/0600 permissions;
  this does not replace ownership, existing-mode or race-safe traversal checks. No position,
  maximization or visibility is persisted. Existing Windows paths and warning
  on save failure remain unchanged. Later Unix wiring must warn on ownership or
  write failure and continue without saved state, and recreate missing cache.

Login startup must preserve each user's opt-out across updates/reinstallation.
Explicit Quit stops ordinary residency for that session; ordinary launchers must
not restart it in a loop. Emergency fallback is a separate policy and may restart
the Client after Quit, subject to verified GUI eligibility. This behavior must
also be explained in the later user-facing installation documentation. The
opt-out storage/detection mechanism is still an OS integration decision.

Later deployment uses existing Kanade jobs and app-packages, with initially
ad-hoc signing on managed Macs. This stage adds no client_releases bucket,
autonomous updater, paid signing or notarization. OS storage/session acquisition,
launcher lifecycle, opt-out persistence and native notification acceptance still
require implementation and real-machine validation before claiming GUI support.
