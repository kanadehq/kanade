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

## 実装境界と残る判断

管理された publisher 信頼配布、rotation・失効・鮮度、Agent の実検証は Stage 2b。
Stage 1b の immutable API、metadata、atomicity、旧 publish・agent rollout は変更しない。
署名付き publication は未統合。署名 metadata を artifact と同じ不可分な公開点で
確定し、冪等／衝突比較にも含める限定的な統合が必要である。可変 sidecar の追加や
確定済み unsigned record の書換えでは代用できない。既存 unsigned record の移行と
鍵 rotation 時の再署名の不変性、OS ごとの最低版の意味、package ID／capability
registry、初回鍵の生成・保管方法は明示的な判断が必要。

## 検証記録

launcher の取得済み base は `07c7a7260fd1470b6a56b1749d46fdda8556ef3d`。
今回の変更は既存実装 `51eda3e2` に追加した。tracked `magi.toml` は存在しない。
`.github/workflows/ci.yml` は全 target の check・test・clippy・locked check、
rustfmt、editorconfig と coverage を実行する。ローカル集約は `cargo make check`。
全 suite・review loop・最終 gate と修正後の再実行は親 run の責任であり、この
作業で実行した focused test を最終 gate の成功とは扱わない。

今回の実行結果（2026-10-20）:

| コマンド | 結果 |
| --- | --- |
| `cargo test --offline -p kanade-shared --lib package_manifest::tests` | 18 件成功（golden bytes、乱数鍵、弱い鍵、null／欠落の拒否を含む） |
| `cargo test --offline --locked -p kanade --test app_manifest_cli` | 6 件成功（乱数鍵、誤鍵、改ざん、弱い鍵、終了コード） |
| `cargo test --offline --locked -p kanade-shared --lib artifact::tests` | 既存 9 件成功 |
| `cargo clippy --offline --locked -p kanade-shared -p kanade --all-targets -- -D warnings` | 成功 |
| `CARGO_NET_OFFLINE=true cargo make notices` | 成功、生成ファイルの差分なし |
| 変更した Rust 3 ファイルへの `rustfmt --edition 2024 --check`、`git diff --check` | 成功 |

macOS の SDK／linker は sandbox 内の一時ディレクトリ照会に関する警告を出したが、
上記テストは実際に実行され成功した。最初の shared テストは追加 dev dependency の
既存 lockfile への反映を伴い、その後は `--locked` を使用した。
backend は bin crate であり、最初の `--lib` 指定はテストを実行せず失敗したため、
`--bin kanade-backend` に修正した。

`cargo test --offline --locked -p kanade-backend --bin kanade-backend api::app_artifacts::tests`
はテスト binary の link 時に exit 101 で失敗した。
`clang: error: unable to make temporary file: Operation not permitted` のため
API テスト本体は未実行であり、回帰テスト成功とは扱わない。backend のビルドは
空の `web/dist` に placeholder を生成したため、実 SPA の検証も含まない。
launcher の一時ファイル作成を許可された環境への修復が必要で、同じ環境での
再試行や権限変更・sandbox 回避は行っていない。

参考として、移植元の既存記録では非ネットワーク app-artifacts 9 件が成功し、
次の明示的な broker 統合テストは `api/agent_releases.rs` の fixture の
`pick port` assertion で起動前に失敗していた（成功 0・失敗 1）。

```sh
cargo test --offline --locked -p kanade-backend \
  api::app_artifacts::tests::platforms_formats_and_standard_object_store_reads_are_isolated \
  -- --ignored
```

これは過去の実測であり、今回の再実行結果ではない。今回は link の段階で
阻害されたため統合テストも未実行。実行環境の修復時には一時ファイル作成に加え、
使い捨て broker 用のローカルポート確保も必要になる。最終 gate は親 run が担う。
