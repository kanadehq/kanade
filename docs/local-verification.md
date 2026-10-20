# ローカル検証の補足

Rust gate は `magi.toml` の既存の 5 コマンドを維持する。e2e は
`python3 scripts/ci/magi-e2e.py` で、既存の publication/store、manifest、
trust/admission、CLI テストを実行する。stage 2a/2b は各
`module::tests::` を個別指定する。Python 3、Rust、CI と同版の
`nats-server`、loopback の bind/connect 権限が必要。
broker は既存 fixture が一時的に起動し、実 fleet には接続しない。
`KANADE_APP_ARTIFACT_PUBLICATION=backend-exclusive` は子プロセスに設定する。

runner は `integration.yml` の `NATS_SERVER_VERSION` を読み、実行版との
不一致を失敗にする。各コマンドの `running N tests` と
`test result: ok. N passed` を集計し、成功が 0 件、ignored が残る、
harness ごとの件数が一致しない場合も失敗にする。Cargo の終了コードが
非ゼロなら、その失敗を保持する。次の回帰テストは外部サービス不要。

```sh
python3 -m unittest discover -s scripts/ci -p test_magi_e2e.py
```

## 空の検証を記録した run の根拠

次の保存済み `run.json` を読み取り専用で確認した。すべて
`config.verify.gate=[]`、`config.verify.e2e=[]`、`gate_ran=true`、
`gate=[]` だった。

| stage | run ID | base commit |
| --- | --- | --- |
| 1b | `20261020-042223-2f3d` | `ac4e4c6857718d04cdb013bc6b30a0b78b66ea83` |
| 2a | `20261020-082352-6dc2` | `07c7a7260fd1470b6a56b1749d46fdda8556ef3d` |
| 2b | `20261020-191934-45e8` | `dc437d63618bdafc74e40ccb9680e1e7fac1a2ae` |

各 base の Git tree には `magi.toml` も `.magi/` も存在しない。
`15fea926` で初めて repo 設定が追加されたため、base から抽出する
repo layer に、その run 自身で追加した設定は含まれなかった。
`gate_ran=true` だけではコマンド実行を示さず、空のローカル検証は
public CI やコードレビューの未実施を意味しない。
この証拠は repo layer の設定欠落を示し、正しい設定を無視したという
上流の不具合の証拠にはならない。現在の launcher の取得済み base は
`65f6be313c214133a8ec0849bc24aeee158e0712` であり、この履歴上の
base と区別する。今回も machine 設定と保存済み run は変更していない。

## ホストで代替できない CI 範囲

- Apple Silicon macOS の `client-macos.yml` は
  `sh scripts/build-client-macos.sh` による production release bundle、
  ad-hoc codesign、macOS 14 最低版の検査と
  `cargo test --locked -p kanade-client --all-targets` を実行する。
  通常の debug Rust gate だけで bundle 検証の成功とは扱わない。
- Linux の `web.yml` は Chromium と OS 依存ライブラリを入れて
  Playwright component screenshot テストを実行する。macOS の実行結果は
  Linux の screenshot baseline を保証しない。
- Windows のサービス・session・ACL・installer・GUI の分岐、
  `client-macos.yml` の Windows client check/test は Windows で確認する。
  macOS gate では Windows のサービス動作を検証できない。
- `integration.yml` の OS matrix による offline boot、NATS 認証切替、
  role conformance、PowerShell/deploy-script テストも別の範囲である。
  focused publication e2e で全 integration の成功とは扱わない。
- editorconfig、coverage、ライセンス・notice・SBOM、docs の専用ジョブは
  引き続き CI 側の確認が必要。既存 workflow の必須検査は変更しない。

## 今回の移植の検証

runner の unittest は 6 件成功（終了 0）。`git diff --check` も終了 0。
設定された e2e コマンドは起動したが、`/opt/homebrew/bin/nats-server` が
2.15.0、CI ピンが 2.14.1 のため版照合で終了 1 になった。Rust の
publication/store・manifest・trust/admission テストには到達していない。
launcher 環境に CI ピンと同版の NATS を用意する必要がある。
親 run が全体検証と最終 gate を所有するため、この移植から別 run は起動しない。
