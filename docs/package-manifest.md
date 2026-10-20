# 署名付き package manifest（Stage 2a）

`kanade_shared::package_manifest` と `kanade app sign-artifact` /
`verify-artifact` / `publisher-trust` はローカルの暗号契約を提供する。
署名・期待する完全な identity・artifact の SHA-256 と byte 数だけを確認する。
現在の publisher 認可・鮮度・失効、宣言と実際のパッケージ内容の適合は確認しない。
成功しても deploy-ready 状態や特権インストールの許可にはならない。

## 正規形と署名 bytes

v1 の全 field は必須で、null・未知 field・重複 field（入れ子を含む）・欠落・
不正／途中までの JSON・末尾データを拒否する。固定順序の compact UTF-8 JSON
のみを受理し、ファイルの末尾 LF は 1 個だけ省略可能。空白・順序・escape の
別表記は拒否する。これは汎用 JSON 正規化規格ではなく、変更時は manifest
version を更新する契約である。

署名対象 JSON の順序と構造は以下のとおり（値は例示）。

```json
{"manifest_version":"1.0","identity":{"role":"cli","version":"1.2.3","os":"macos","arch":"aarch64","distro_abi":"darwin","format":"tar_gz"},"package_id":"com.kanade.cli","min_os_version":"14.0","required_agent_capabilities":["a_cap","b_cap"],"sha256":"2e547448dcd0f2fcd9dbc386d33f1553369883451898177559bcf3e3b1083d16","size_bytes":13,"publisher":{"algorithm":"ed25519","key_id":"ed25519:c283f73433913d171beef378f847ac83bb4bfe58359cd7a7dd6d295b13562b87"}}
```

ASCII `kanade/package-manifest/ed25519/v1`、NUL 1 byte、上記 compact JSON
（末尾 LF なし）を順に連結し、Ed25519 で直接署名する。ファイルには
`publisher.key_id` の後に `signature`（64 byte の標準 padded base64）を追加する。
`algorithm` と `key_id` も署名対象。version は `1.0`、algorithm は `ed25519`
のみ。検証は `verify_strict` を使用する。golden テストは key_id と digest を
含む署名 bytes 全体を固定文字列で検証する。

`key_id` は `ed25519:` + SHA-256 の全 64 桁 lowercase hex。
ハッシュ対象は ASCII `kanade/package-publisher-key-id/v1`、NUL 1 byte、
32 byte の公開鍵の順である。上例はテスト専用の固定 seed に対応する。

identity は既存 `ArtifactIdentity` の全 6 field と正規 SemVer・対応 platform
検証を再利用する。空 artifact は拒否する。`package_id` は `[a-z0-9]` で始まる
`[a-z0-9._-]` の 1–128 byte。`min_os_version` は非 null の必須文字列で、
1–4 個のドット区切り十進数（各 1–9 桁）。capability は各 1–64 byte の
`[a-z0-9_]`、最大 32 件、重複のない厳密昇順（CLI がソート）。空配列は可能。
OS ごとの互換性判定、package ID と capability の registry は未実装。

## 鍵管理と独立した信頼入力

publisher 鍵の管理者が command 署名用とは別の秘密鍵を保管し、信頼情報の
配布担当者が認証された独立経路で公開 trust record を検証者へ渡す。
検証者はその取得経路と期待する identity の選択に責任を持つ。
用途別の型と domain 分離は秘密鍵の意図的な使い回しを検出できない。
この CLI は鍵生成・保管サービス・live provisioning を追加しない。

`publisher-trust` の公開出力は `purpose`（`kanade-package-publisher-key-v1`）、
`algorithm`、`key_id`、`public_key`（32 byte の標準 padded base64）を持つ。
検証時は用途・algorithm・有効な公開鍵・fingerprint の一致を確認し、弱い公開鍵を
明示的に拒否する。trust record は所有者管理のディレクトリに置き、不正な
置換を防ぐ書込権限／ACL を設定する。manifest や package に付属する鍵、同じ
配布元から同時に届いた鍵だけを信頼根拠にしてはならない。

秘密鍵入力は 32 byte Ed25519 seed の標準 padded base64。既存の非表示 prompt、
または `--secret-stdin` だけを使用し、argv 値・環境変数・manifest・app metadata・
job に秘密鍵を入れない。既存の保護された secret store から読み出す。
ファイル利用時は署名アカウントが所有し、Unix 0600 と所有者専用ディレクトリ、
または同等の Windows ACL を要求する。共有・同期領域やソース管理に置かず、
shell tracing・端末録画・履歴への秘密の記入も避ける。入力 buffer の消去は
best effort であり、端末や decoder を含むすべてのコピーの安全な消去は保証しない。

## 使用例と終了コード

`identity.json` は既存の明示 `ArtifactIdentity` JSON。秘密鍵は非表示 prompt で入力する。

```sh
kanade app publisher-trust > trusted-publisher.json
kanade app sign-artifact --identity identity.json --package-id com.kanade.cli \
  --min-os-version 14.0 --capability a_cap --capability b_cap \
  --out package.manifest.json package.tar.gz
kanade app verify-artifact --trusted-key trusted-publisher.json \
  --identity identity.json package.manifest.json package.tar.gz
```

trust record の export は署名側で行い、検証側には独立した認証経路で配布する。
自動化では保護された secret store の reader から stdin へ直接流し、署名／export
コマンドに `--secret-stdin` を付ける。秘密そのものをコマンド行へ書かない。

署名出力は排他的 create で既存ファイルへの上書きを拒否する。書込失敗時は
不完全な出力を削除する処理を試みる。artifact はストリームでハッシュする。
すべてローカルで動作し backend・NATS・Object Store・自動更新確認は使わない。
終了コード 0 は操作成功、1 は I/O・入力内容・検証失敗、2 は clap 引数不備。
`--min-os-version` の欠落は 2、null や改ざん manifest の検証は 1。
成功表示 `OFFLINE VERIFIED (signature + integrity only)` は限定された結果を表す。

## Stage 2b: 管理された publisher 信頼と admission

実装は `kanade_shared::publisher_trust`（信頼状態・永続化）と
`kanade_shared::package_admission`（admission）。manifest 契約は変更しない。

### 信頼モデル

1. 管理者が trust root（専用の Ed25519 公開鍵、`purpose =
   kanade-package-trust-root-v1`）を `trust-store-provision` で各ホストに pin する。
   これだけが信頼の起点で、NATS・Object Store・package・manifest の鍵から
   初回信頼（first-use）することはない。`apply` は root を変更せず、異なる root の
   provision は `RootAlreadyPinned` で拒否する（同一 root の再実行は冪等）。
   root が無いホストは何も受理しない。
2. root 鍵の保持者が trust state を署名する。署名 bytes は
   ASCII `kanade/package-trust-state/ed25519/v1`＋NUL＋固定順 compact JSON。
   manifest の domain と異なるので相互に流用できない。root 鍵・publisher 鍵・
   command 署名鍵は別権限。root 鍵を publisher として列挙する state は拒否する。
3. Agent は state を pin 済み root でのみ検証し、manifest の `key_id` で
   管理下の鍵を選び、既存の `verify_offline` で manifest を検証する。

### trust state（`state_version` 1.0）

```json
{"state_version":"1.0","trust_set_id":"fleet","sequence":3,"issued_at":1800000000,"not_after":1802592000,"root_key_id":"ed25519:…","publishers":[{"key_id":"ed25519:…","public_key":"<base64>","not_before":1799990000,"not_after":1801000000}],"revoked":["ed25519:…"],"signature":"<base64>"}
```

時刻は unix 秒。`publishers` と `revoked` は `key_id` の厳密昇順（重複不可）、
`publishers` は最大 16、`revoked` は最大 256。`key_id` は公開鍵から再計算して照合する。
`not_after − issued_at` は最大 90 日。`sequence` は 1 以上。空白・順序・escape の
別表記、未知／重複 field は拒否する。

更新規則（永続化の前に検査）:

* `trust_set_id` は保存済みと同一。`sequence` が大きいときだけ前進する。
  同一 sequence・同一 bytes は冪等な no-op、低い sequence は rollback、
  同一 sequence で内容が異なれば conflict。`issued_at` は後退不可。
* revoked は累積。後継 state は先行 state の revoked をすべて含む。
  失効した鍵の復活は不可（新しい鍵へ rotation する）。
* rotation: 新旧鍵の期間を重ねた state を配り、全ホストへ反映した後に
  旧鍵を revoked に入れた state を配る。
* 既に `not_after` を過ぎた state、未来（許容 5 分超）に発行された state は適用不可。

admission 時の publisher 認可は、state が未失効の `not_after` 前であり、鍵が
active・非失効・自身の `[not_before, not_after)` 内であること。

### 永続化と時計の不確実性

trust directory（既定 `<data_dir>/package-trust`）に `root.json` と
`state.json` を置く。`state.json` は署名済み state と時刻 high-water を持つ
1 レコードで、private な一時ファイル→fsync→rename→ディレクトリ fsync により
原子的に置換する（クラッシュ時は旧／新のどちらか、残った一時ファイルは無視）。
保存アダプターは Unix のみ対応。作成時は 0700/0600。読込時に symlink・group/other
アクセスを拒否する。Unix では実行ユーザー所有・通常ファイル・リンク数1を確認し、
`O_NOFOLLOW` で開いた descriptor の属性も検査する。trust directory は
相対成分・symlink を含まない絶対パスを指定する。祖先は root または実行ユーザー
所有とし、他者が書ける祖先は sticky bit が必要。macOS では保存先・祖先の
extended ACL も拒否する。Unix の root pin・state 適用・high-water 更新は
同じ `write.lock` の `flock` でプロセス間も直列化する（このファイルは削除しない）。
Windows を含む非 Unix では、owner／DACL／祖先の保護・プロセス間排他・耐久性のある
原子的置換を保証できる保存アダプターが未実装のため、provision はディレクトリ作成前に
拒否する。既存の root/state があっても読込・適用・admission を `Broken` で拒否し、
status は `broken` を返す。未設定時は `no-root` のまま、既存の Agent 動作は維持する。
手動 ACL 設定や管理操作の直列実行でこの制限を解除することはできない。
各操作で毎回読み直し、pin 済み root で再検証するため、
失効は再起動なしで反映される。破損・改ざん・別 root 署名のレコードは
`Broken` として fail closed し、初期状態へ黙って戻さない。復旧は管理者が
調査して明示的に行う（root が必要な場合は provision をやり直す）。

admission は以下で拒否する（既存の Agent 動作には影響しない）:
wall clock が high-water より 5 分超遅れる、monotonic clock との乖離が 5 分超、
high-water（1 時間以上進んだとき永続化）を保存できない、state の期限切れ。
再起動直後は monotonic 基準が新しいため、永続 high-water が時計後退の検出を担う。
時計を正しい値へ戻すか、新しい state を適用すれば再開する。

保証しない範囲: 管理者／root 権限者による trust directory 全体の巻戻し
（sequence と high-water も一緒に戻る）、時計と files の同時改変、manifest 自体の鮮度
（manifest は署名時刻を
持たない。古い版への巻戻しは呼び出し側が期待する identity／version で防ぎ、鍵の
危殆化は失効で止める）。

### admission API

`admit_package(store, AdmissionRequest)` は次をすべて要求する: 外部指定の
digest／size と artifact bytes の一致、現在有効な publisher 認可、
外部指定の完全な `ArtifactIdentity` に対する `verify_offline`、期待 `package_id`、
`CompatibilityPolicy`（platform、数値比較の OS 下限、必要 capability）。
結果の `AdmittedPackage` は identity・artifact digest・検証した manifest bytes の
SHA-256・trust sequence に束縛された証拠で、
`OfflineVerified` から作れず、install 権限ではない。typed handler は自前の特権確認を
別途行う。

### 管理 CLI（ローカル専用、`kanade app`）

| コマンド | 役割 |
| --- | --- |
| `trust-root-record` | root 秘密鍵（非表示 prompt／`--secret-stdin`）から公開 root record を出力 |
| `trust-state-sign --draft --out` | draft（`trust_set_id`・`sequence`・時刻・publishers・revoked）を root 鍵で署名。既存ファイルは上書きしない |
| `trust-store-provision --root-record [--dir]` | 管理者が root を pin する明示操作 |
| `trust-store-apply --state [--dir]` | pin 済み root で検証し、規則を満たす場合のみ原子的に保存 |
| `trust-store-status [--dir]` | root／state／健全性（`ok`・`no-root`・`no-state`・`expired`・`clock-uncertain`・`broken`）を公開識別子と時刻のみで表示 |

秘密鍵は argv・環境変数・job・ログに出さない。live key は自動変更しない。

運用手順（管理者）: ① root 鍵を publisher 鍵・command 署名鍵とは別に生成・保管し
`trust-root-record` で公開 record を得る。② 認証された独立経路で各ホストへ渡し、
key_id を確認して `trust-store-provision`。③ publisher の `publisher-trust` 出力から
draft を作り `trust-state-sign`、各ホストで `trust-store-apply`。④ 期限前に
sequence を上げた state を再署名・適用。⑤ rotation／失効は上記規則に従う。

### 実装境界と残る判断

* **配送は blocked**: 既存の管理経路（command 鍵 provisioning と NATS KV）には、
  pin 済み root とは独立に認証された trust state の配送口が無い。本変更は暗黙の
  bootstrap を作らず、ローカルの provision／apply／status と検証境界のみを実装した。
  state は root 署名自体で認証されるため将来 NATS 等で運んでも信頼は増減しないが、
  Agent 常駐処理への組込み・自動 apply・heartbeat 報告は別タスク。
* 特権配備、typed command 配送、job 実行、install／update、Agent self-update、
  CI 署名秘密、app artifact の publication は変更しない。既存の artifact
  publication は既定無効のまま。
* 署名付き publication は未統合。署名 metadata を artifact と同じ不可分な公開点で
  確定し、冪等／衝突比較にも含める限定的な統合が必要である。可変 sidecar の追加や
  確定済み unsigned record の書換えでは代用できない。既存 unsigned record の移行と
  鍵 rotation 時の再署名の不変性、OS ごとの最低版の意味、package ID／capability
  registry、初回鍵の生成・保管方法は明示的な判断が必要。

## 検証記録（Stage 2b）

launcher が固定した base は `dc437d63618bdafc74e40ccb9680e1e7fac1a2ae`。
repo-owned の `magi.toml` を追加し、`verify.gate` を `.github/workflows/ci.yml` の
check・test（`--all-targets` と `--doc`）・rustfmt・clippy・locked check と一致させた
（editorconfig と coverage は CI 側の専用ツールのため含めない）。`verify.e2e` は
本機能の focused テストを実行する。NATS を要する `integration.yml` の `--ignored`
テストは opt-in のままでここには含めない。kata 管理の workflow は変更していない。

この作業で実行した focused コマンド（すべて実テスト実行。コンパイルのみではない）:

| コマンド | 結果 |
| --- | --- |
| `cargo test --offline -p kanade-shared --lib -- publisher_trust package_admission package_manifest` | 43 件成功（trust 17、admission 8、manifest 18） |
| `cargo test --offline --locked -p kanade-shared --lib` | 733 件成功、6 件 ignored（既存） |
| `cargo test --offline -p kanade --test app_trust_cli --test app_manifest_cli` | 2 件＋6 件成功 |
| `cargo test --offline --locked -p kanade --bin kanade` | 145 件成功 |
| `cargo clippy --offline --locked -p kanade-shared -p kanade --all-targets -- -D warnings` | 成功 |

socket／sandbox の阻害はこれらの実行では発生しなかった（テストは実 socket を使わない）。
全 suite・review loop・最終 gate と修正後の再実行は親 run の責任であり、上記を
最終 gate の成功とは扱わない。
