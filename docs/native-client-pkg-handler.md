# macOS Client pkg handler: local foundation

`crates/kanade-agent/src/native_install/client_pkg/` implements the isolated
Client pkg transaction state machine for SPEC §2.11.7/§2.13. It uses the
payload-only packaging contract and the native CLI installer's Unix filesystem
primitives, without reusing the CLI's executable swaps or rollback logic.
The launcher-provided fetched base was
`bdfb416df16ddb45bdeddc9f8017fe58a2d5d2ef`; no additional fetch was required or
performed. This records a snapshot, not remote freshness after that fetch.

## Readiness boundary

There is no production entry point. Writable roots must be isolated temporary
directories. `NativeSystem::for_image` accepts only a canonical temporary image
root; its target must match the handler's Applications directory. The native
adapter cannot acquire trustworthy launch inhibition or prove an interrupted
system installer transaction quiescent. It therefore defers **all native
mutations**, including fresh install, rather than claiming safe sessions from
an empty process list. The installer invocation is represented by fixed
`/usr/sbin/installer` with structured `-pkg`/`-target` arguments and a bounded
runner; no native installer invocation was exercised. The eventual live
contract remains the fixed system volume and expected Client app location.

`HandlerUnavailable`, backend typed dispatch, the empty heartbeat handler
registry and `END_TO_END_DEPLOY_READY = false` remain in force. No generic
shell fallback, deploy CLI, artifact-store connection, live keys, bootstrap
changes, LaunchAgents, notifications, tray integration or GUI restart were
added. Successful generic-shell operational installation of a previously built
Client package is background evidence only; it does not authorize this handler
or substitute for its approval, admission and coordination gates.

## Authorization and validation

`Request` carries a separately approved immutable plan, exact typed operation,
job/exec identifiers and target. `Authorization::current` is a trusted local
adapter contract: it must reload current gate inputs for every call, rather
than return a cached Approval. The state machine evaluates the shared plan gate
under its lock, before installer, after installer, and during reconciliation
and rollback. It pins the original plan ID/digest/target. Production transport,
replay ledger and outbox integration are still a separate prerequisite.

Every admission independently rehashes actual pkg bytes and binds identity,
object key, size/digest, signed manifest digest and package ID to the typed
operation. The component ID must also equal the fixed ID inside PackageInfo.
Admission reads the independent publisher trust store, including revocation,
freshness and compatibility, again at mutation boundaries. The unsigned pkg
and ad-hoc app signature establish no publisher identity, Developer ID,
notarization or Gatekeeper trust.

Before installer, the bounded Rust XAR/zlib/gzip/odc-cpio readers reject extra
members, executable Distribution attributes/elements, scripts, links, relocation,
unsafe ownership/modes, extra bundle files and inconsistent versions/identities.
Only the current fixed productbuild metadata shape is accepted. The app must
have the fixed identity/executable, macOS 14.0 minimum and thin arm64 Mach-O.
XML Info.plist is required; unsupported representations fail closed.

The OS boundary uses fixed absolute `lsbom`, `codesign` and `pkgutil` paths,
structured arguments, empty environment, null stdin and nonblocking bounded
output. Read commands have 10/20 second limits; installer has a 300 second
limit; combined stdout/stderr is capped at 128 KiB. BOM paths, owners, modes,
regular-file sizes and POSIX cksums must match the parsed payload before root
installer could execute. `codesign --verify --deep --strict` plus display
verification checks app integrity, ad-hoc signature and signature identifier.
No native extraction of an unvalidated pkg is used.

AppleDouble payload support is deliberately narrow: the observed current
pkgbuild shape with empty Finder info/resource fork and exactly the 11-byte
`com.apple.provenance` attribute. Other shapes/attributes are refused. These
carriers are validated before being omitted from the scratch app; native
installer would interpret them as attributes. BOM/receipt carrier references
must point to real entries. The metadata fixtures were extracted from an
actual package built with the existing packaging test helper, with a tiny test
executable, not the production Client. No binary containing build-host metadata
is committed. A native test generates a fresh package through the same Python
validator, then applies the Rust validator to those exact bytes.

## Transaction and recovery

The handler holds a bounded (at most 60 seconds) advisory flock in the private
deploy directory, `client.pkg.install.lock`, for its whole transaction. This
serializes cooperating managed Client operations; it does not own the OS
installer database or exclude unrelated root administrators. State/cache access
reuses no-follow dirfd operations, owner/mode/ACL and link checks, exclusive
creation, fsync and atomic replacement of **our own JSON state only**. The
Client lock is separate from the CLI lock, following the foundation's local
layout rather than asserting the proposed production runtime-lock layout.

The private `client-pkgs` cache retains content-addressed verified pkg bytes.
A managed receipt records the operation, signed manifest, full bundle content
digest, plan, job/exec and target. No unmanaged app or matching OS component
receipt is adopted: DeployClientSpec has no adoption approval schema. An update
requires the current bundle, OS receipt and managed record to agree and the
previous cached pkg to remain structurally valid and currently admitted.

Journal phases are `prepared`, `installing` (intent), `installer_returned`,
`rolling_back` (intent), `committed`, `rolled_back`, `abandoned` and
`recovery_required`. Installer success alone is insufficient. Installed/no-op
verification reads the OS receipt and full bundle, including ownership, modes,
bytes, signature and inode continuity. The result's GUI field is always
`unsupported_not_attempted`; it makes no GUI eligibility, launch or KLP claim.

A LaunchLease must inhibit all managed launch/fallback paths for its lifetime,
respect Quit/opt-out, and positively establish that no active managed instance
can race the mutation. Path, PID/start time and UID are required for any real
process evidence. This slice has no process termination or session launch API.
The fake lease is deterministic test evidence only. Native coordination is
unavailable, so users' processes and preferences are never changed.

On a positively quiescent failed update, rollback is attempted only if the new
installation is fully identifiable. It re-admits the retained previous pkg and
requires a **separate current approval for the previous typed operation**; the
new plan does not implicitly authorize installing an older artifact. The prior
operation's authority adapter must supply a currently valid recovery approval,
not infer one from an old managed receipt. The recovery plan ID/digest is
journaled and rechecked before invoking installer. Rollback uses the same OS
installer adapter, never raw app replacement or OS receipt/database editing.
There is at most one rollback attempt and at most three forward attempts for
the same approved execution; a rolled-back execution is not automatically
reinstalled. No automatic installer retry loop is used.

Timeout, unknown worker lifetime, partial/divergent files or receipts, revoked
approval/trust, or failed rollback become `recovery_required`. The journal and
cached evidence remain and subsequent calls freeze. Fresh failure never
removes an app or OS receipt to emulate uninstall. `recover` can record revoked
approval/trust without a still-valid copy of the original request. Restart
reconciliation requires current authorization/trust and independent OS
transaction quiescence before interpreting observed old/new state. It can
commit an exactly verified new state or record a verified previous state;
ambiguous or interrupted rollback is frozen rather than layered with another
installer transaction. These transactions are **not atomic**.

Successful validation currently retains private scratch evidence as well as
packages; there is no automatic cache/scratch garbage collection or operator
unfreeze API. Both require explicit integration policy before live use.

## Verification and pending acceptance

Focused checks for this change:

```sh
cargo test -p kanade-agent --bin kanade-agent native_install:: --locked
cargo clippy -p kanade-agent --all-targets --locked -- -D warnings
python3 -B -m unittest discover -s scripts/ci -p 'test_client_macos_pkg.py'
python3 -B -m unittest discover -s scripts/ci -p 'test_magi_e2e.py'
```

The required `scripts/ci/magi-e2e.py` pipeline now runs the Client module as its
own nonempty harness, in addition to the existing CLI foundation checks. The
repository's nonempty gate commands in `magi.toml` remain unchanged. The parent
run owns full e2e, complete verification, review and the final gate; these are
not replaced by the focused checks above.

Observed focused results on the implementation host: 103 native-installer
Rust tests (25 Client pkg tests), two capability-barrier tests, 26 Python
packaging tests and 14 e2e-runner unit tests passed; agent all-target clippy
with warnings denied and targeted rustfmt checks passed. The e2e-runner unit
tests use mocked broker downloads; they are not the full e2e run. The parent
run must report its final reviewed/tested commit and gate outcome separately.

Real checks: no-follow filesystem operations, macOS ACL tests, flock, durable
JSON/cache writes, signed publisher trust/admission, shared plan evaluation,
real `pkgbuild`/`productbuild`/BOM and ad-hoc app-signature validation through the
existing packaging fixture. Mutated native package resources are rejected by
the additional BOM size/checksum check before installer.

Simulated checks: installer and OS receipts, process/launch lease, clocks,
install/update/no-op/rollback, trust revocation at mutation boundaries, current
approval revocation/expiry, unmanaged ownership conflicts, drift, symlink/path
replacement, lock contention, interruptions and failed recovery. Temporary
keys are fixture keys only. These do not demonstrate OS installer transaction
behavior or real user sessions.

No native install/update/rollback smoke was run: no genuinely disposable VM or
image with an isolated OS receipt database was provided. Clean macOS 14+
Apple Silicon acceptance remains pending, including real production pkg fresh
install/update/recovery and system receipt reconciliation, root:wheel/ACL and
quarantine behavior, native process/launch inhibition, opt-out/Quit preservation,
and separately verified eligible-user GUI/KLP behavior. No operator app,
process, receipt database or deployment job was changed.

## 追加した結果・境界試験

`client_pkg::result::ResultRecord` は既存の `Report` / `Error` を構造化し、
導入検証状態、バージョン、GUI 状態、`FailureClass` を個別に記録する。
成功と no-op の双方で receipt/bundle の検証を示すが、GUI 起動や KLP 接続は
示さない。中断や段階不明の I/O エラーは保守的に `recovery_required` と分類する。
これはローカルの結果モデルであり、結果送信経路は追加していない。

ネイティブの `ProcessView` は実行パスが一致する Client の PID・UID・開始時刻を
読み取る。同名の別パスは Client と判定せず、実行パスや Client の識別情報が
取得できない場合は defer する。空の観測結果も launch lease の代わりにはならず、
既存の launch coordination 不足による全ネイティブ変更の defer は維持する。
プロセス分類試験は決定的な観測データを使う。

runner 試験は実際の `/bin/sleep`、`/usr/bin/yes`、`/usr/bin/env`、
`/usr/bin/printf`、`/usr/bin/false` を使い、時間・出力上限、空環境、
特殊文字を含む argv の保持、終了失敗と起動失敗を確認する。これは OS installer
の試験ではなく、receipt データベースや `/Applications` を変更しない。
既存 e2e の `native_install::client_pkg::` フィルタが追加試験も対象にする。
実 pkg の新規導入・更新・復旧については、引き続き使い捨ての clean-Mac 環境での
受け入れ確認が必要である。
