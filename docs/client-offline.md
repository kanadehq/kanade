# Client App offline operation

The end-user Client App talks only to the local Agent (KLP). When the Agent has
no NATS/backend connectivity it can still show the **last synchronized** job
catalog and notification history, and — only for jobs that explicitly opt in —
run jobs. Viewing and executing are separate decisions.

## Policy summary

| Question | Answer |
|----------|--------|
| May a job run offline? | **No by default.** Only `client.offline.allow: true`. A manifest with no `offline` block (every existing manifest) or `allow: false` is never run offline. |
| How long after the last sync? | `client.offline.max_age`, **1s – 24h**, whole seconds, required when `allow: true`. |
| Counted from? | The **start** of the last successful *authoritative* sync (never extended by mere reconnection or a partial sync). |
| Does `staleness: strict` still apply? | Yes. The shorter of `offline.max_age` and `staleness.max_cache_age` wins. `cached` / `unchecked` add no extra bound. |
| After an Agent restart? | Browsing is restored from disk. **Execution stays denied until the next successful sync**, because a stored timestamp cannot prove how long the machine was off or whether the clock changed. |
| Online behaviour? | Unchanged. A reachable broker is always authoritative: an online `NotFound`, refusal or error is never answered from the cache. Agent-scheduled and operator executions are untouched. |

```yaml
client:
  name: "Clear temp files"
  category: troubleshoot
  offline:
    allow: true
    max_age: 1h      # 1s..24h; omit the whole block to deny offline runs
```

`allow: false` with a `max_age`, `allow: true` without one, a value outside
1s–24h, sub-second values and unknown keys are rejected by `Manifest::validate`
/ the schema (`docs/schemas/job.schema.json`).

## What an offline run requires (all of it)

1. `client.offline.allow: true` with a valid `max_age`.
2. 定義と必須スクリプト本体（inline または `script_object`）がローカルに保存され、
   SHA-256 が一致すること。Object Store 本体は同期時にdigestを確認して保存し、
   オフライン実行では保存済みバイトだけをinline化する。欠落・破損時は拒否する。
   起動直前にも認可・有効期間・定義世代を再検証する。
3. Not revoked (`script_status`) and visible to this PC (`client.visible_to`
   against the cached group membership) as of the last sync.
4. In-process freshness evidence for the loaded snapshot, within the validity
   period. Wall clock and the process monotonic clock are both used: a clock set
   backwards, a sync "in the future", or evidence that does not belong to the
   loaded snapshot all deny. Suspend (wall ahead of monotonic) counts the larger
   elapsed time.
5. The job can complete locally. Jobs needing the broker/backend are refused
   with `needs_broker` even when `allow: true`: `require_approval`,
   `deploy_authorization`, `collect`, `finalize`, `check`, `emit`, `inventory`,
   `feed` and typed deployments.
   macOS以外の別ユーザーセッション経由の起動も、起動直前ガードに対応しないため拒否する。

Anything absent or uncertain denies (fail closed). Each Client row shows the
reason code and message (`offline_not_allowed`, `sync_expired`,
`resync_required`, `revoked`, `not_visible`, `definition_missing`,
`script_missing`, `integrity_mismatch`, `clock_anomaly`, `needs_broker`, …) and the Client shows
an offline banner with the last synchronization time.

## What is cached, and where

`<data_dir>/client_cache/` (directory `0700`, files `0600` on Unix; symlinks,
non-regular and group/world-writable files are ignored; atomic replace):

Windowsでは継承ACLを無効化し、Agent実行アカウントとSYSTEMだけにアクセスを許可する。
保存前にディレクトリを制限し、既存キャッシュのファイルにも同じACLを適用する。
権限設定に失敗した場合はキャッシュを使用しない。

* `catalog-*.json` — user-facing manifests (raw bytes + SHA-256), revocation
  set, effective group membership, sync start time. Per broker + PC scope.
* `notifications-*.json` — the user's complete notification history and read
  marks. Per broker + PC + OS user, so users never see each other's data.

Capacity: ≤ 2000 manifests (a larger catalog fails the sync and keeps the
previous snapshot), newest 500 notifications, 32 MiB per file.
定義の取得中から件数とJSONエスケープ後の累積バイト数を制限する。
スクリプト追加で上限を超えた同期も失敗し、実行用の鮮度証拠を失効させる。

Digests detect accidental corruption, **not tampering**; protection of the cache
rests on its OS permissions.

## Synchronization and reconnect

同期タスクは起動後・再接続時・接続中の5分周期で実行し、失敗時は30秒後に再試行する。
カタログ、両グループ所属KV、`script_status`の全件読み取りが成功した場合だけ同期を
確定する。部分的な失敗を「失効なし」「所属なし」と扱わない。閲覧用の前回スナップショットは
保持するが、実行の鮮度証拠は同期開始時に無効化する。成功時はスナップショットを置換し、
削除・失効・所属変更を反映する。同期前後の各KVストリームのsequenceなどが異なる場合は、
同期中の変更として確定を拒否する。Object Store本体のdigestも最後に再確認する。
通知履歴はオンラインで一覧を取得するときに更新し、全件リプレイ成功後だけ置換する。
オフライン時のack/unackはAgentで拒否し、Clientのボタンも無効化する。

`show_when`のチェック結果はカタログスナップショットに保存し、再起動後も表示判定に
利用する。これは閲覧専用の証拠であり、実行認可には使わない。Clientは接続状態の
変化時と30秒周期でジョブ・通知を再取得する。通知の`retained_ids`はページとは独立した
保持ID一覧で、ページ外の通知を誤削除せずにrecallを反映する。

Nothing about a user's run request is stored or replayed: a request that
arrives while offline either starts once (and its result goes through the
existing durable result outbox, which is idempotent on delivery) or is refused.
A lost response means "unknown", never an automatic retry.

## Known limitations

* Revocations or membership changes made while the PC is disconnected reach it
  only after reconnect; `max_age` (≤ 24h) bounds that exposure. Pick the
  smallest value that serves the job.
