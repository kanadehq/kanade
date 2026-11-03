# macOS Client pkg handler: local foundation

`crates/kanade-agent/src/native_install/client_pkg/` implements the isolated
Client pkg transaction state machine for SPEC §2.11.7/§2.13. It uses the
payload-only packaging contract and the native CLI installer's Unix filesystem
primitives, without reusing the CLI's executable swaps or rollback logic.
今回の launcher 検証済み base は
`01eff06f3f00690551e69367536f6f0c67e05a7d`（2026-10-31 の fetch snapshot）。
その後の remote の不変性を示すものではない。

## 実装境界

ローカルの `NativeSystem::managed_host` と Client の起動前処理に、macOS の
launch exclusion、認証済み session helper、管理対象だけの停止・復帰を実装した。
本番 backend の typed dispatch、deploy CLI、heartbeat handler registry、
`HandlerUnavailable` と `END_TO_END_DEPLOY_READY = false` は維持する。
認証済み command・承認・replay・結果送信の接続を完了したとは主張しない。
汎用 shell install の過去の受入は、この handler の実行承認にもならない。

pkg は引き続き app だけの payload であり、root script や LaunchAgent を追加しない。
新 Agent を再起動していない環境でも手動起動できるよう、Client は durable gate
作成前には既存の管理者所有 `/Library` directory の共有 flock を保持する。
root coordinator は durable closure を先に保存し、Client 停止後に通常の lease と
この移行用 flock の両方を排他取得してから mutation intent を記録する。
作成と起動が競合しても reader の再確認と二つの排他が installer との競合を防ぐ。
部分作成・不正 owner/mode/ACL・読取不能を「ゲート無し」として許可しない。

`client-lifecycle/closed.json` は transaction digest と mutation bit を持ち、fsync
して保存する。writer の drop、Agent 終了、再起動、PID 消失だけでは解除しない。
同一 transaction の journal と実際の bundle/receipt を再照合して解除する。
変更意図のない失敗では、承認失効後でも旧状態の一致を確認して abandoned にできる。
変更意図のある処理には既存の承認・trust・quiescence・rollback 検証が必要であり、
`recovery_required` は自動解除しない。復旧時に失った helper 接続から復帰意図を
再構成することはない。

固定 bundle/executable の owner・inode と kernel audit token（UID、session、PID
世代）を検証する。helper の peer は既存 KLP UID admission を再利用するが、
KLP に GUI session admission を追加しない。helper/GUI の親子関係と同一 UID/session、
応答の鮮度・単調 sequence を検証し、未登録/legacy Client が一つでもあれば停止前に延期する。
停止は helper 経由で要求し、5 秒後に SIGTERM、さらに 5 秒後に SIGKILL、最後に 2 秒待つ。
各 signal は捕捉した正確な identity を syscall 境界で再検証する。

復帰は更新前に稼働した session の helper だけが判断する。Observe と launch 直前に
active/unlocked・Quit・opt-out を再確認し、失効・不明・logout・SSH/headless は起動しない。
helper は最初から対象 user の UID/GID/groups と bootstrap context で動作し、root の
`launchctl asuser` だけで GUI を作らない。固定 argv `--tray`、空環境に account 由来
HOME と検証した Darwin TMPDIR と固定 PATH だけを設定する。
Quit marker と永続 opt-out はユーザー所有 state 内の別ファイルで保持し、更新で消さない。
Quit marker 保存後の helper 通知失敗は Quit を阻止しない。
既存の startup/reveal、second-instance、deep-link、emergency fallback の入口で同じ
ゲートを再確認する。未実装の login 登録や native 通知全体を制御するとは主張しない。

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
bytes, signature and inode continuity. 結果は導入検証と GUI activation を分離する。`not_launched`、
`restore_launched_unverified`、部分復帰を記録し、spawn 成功を GUI/KLP 検証済みにしない。

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

OS mutation intent 記録後の timeout、unknown worker lifetime、部分的/不一致の
bundle/receipt、approval/trust 失効、rollback 失敗は `recovery_required` となる。 The journal and
cached evidence remain and subsequent calls freeze. Fresh failure never
removes an app or OS receipt to emulate uninstall. `recover` can record revoked
approval/trust without a still-valid copy of the original request. Restart
reconciliation は OS mutation intent 記録後、現在の authorization/trust と独立した
OS transaction quiescence を要求してから旧/新状態を解釈する。 It can
commit an exactly verified new state or record a verified previous state;
ambiguous or interrupted rollback is frozen rather than layered with another
installer transaction. These transactions are **not atomic**.

Successful validation currently retains private scratch evidence as well as
packages; there is no automatic cache/scratch garbage collection or operator
unfreeze API. Both require explicit integration policy before live use.

## 検証と未実施の受入

使い捨て環境の手順と記録項目は
[macOS Client pkg + coordination smoke](client-macos-coordination-smoke.md) を参照。

必須 e2e の `scripts/ci/magi-e2e.py` は Client pkg 全体、shared gate、Client の
startup/storage/lifecycle を nonempty harness として実行する。追加した adapter 試験も
同じフィルタに含まれる。親 run が complete suite、e2e、最終 gate と最終 head を所有する。
この修正席は対象 test と clippy/format を実行し、実際の結果を応答に記録する。

ネイティブに実行する対象は一時ディレクトリでの flock、no-follow、権限、永続 state、
Unix socket 切断処理と spawn しない command 構築試験。加えて shared lifecycle の
既存 native 試験は、一時 synthetic bundle に複製したテスト専用子プロセスだけに
audit-token signal を送信し、PID 世代の不一致を拒否することを確認した。
セッションの二 UID/fast switching、
lock/logout、Quit/opt-out、PID 再利用、未登録 Client、graceful timeout、installer/receipt と
transaction の復帰結果は偽アダプターで検証する。これは本物の Aqua session や pkg
installer transaction の検証ではない。

この修正では operator の Client/Agent の停止・復帰、実 pkg 導入、live key provisioning、
queue/question の変更を実施していない。使い捨て VM での native pkg + GUI smoke は未実施。

この修正席での最終 focused 結果: Client pkg 42 件、Client lifecycle 3 件、
Client storage 1 件、shared lifecycle 8 件、e2e runner 単体 15 件が成功。
shared の実出力は required e2e の nonempty/count 検査も成功した。
対象三 crate の all-target clippy（`-D warnings`、`--locked`）、workspace format、
`git diff --check` も成功した。Client pkg の native fixture 試験は test-owned の
pkgbuild/productbuild/BOM/ad-hoc signature の読取検証であり、OS installer は実行しない。
初回の fixture/lint エラーは修正して再試験した。全 e2e・最終 gate・修正後の再レビューは
親 run の担当であり、この focused 結果だけで完了したとは扱わない。
