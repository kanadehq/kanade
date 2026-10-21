# ローカル検証の補足

Rust gate は `magi.toml` の既存の 5 コマンドを維持する。e2e は
`python3 scripts/ci/magi-e2e.py` で、既存の publication/store、manifest、
trust/admission、CLI テストを実行する。stage 2a/2b は各
`module::tests::` を個別指定する。Python 3、Rust、CI と同版の
`nats-server`、loopback の bind/connect 権限が必要。
broker は既存 fixture が一時的に起動し、実 fleet には接続しない。
`KANADE_APP_ARTIFACT_PUBLICATION=backend-exclusive` は子プロセスに設定する。

runner は `integration.yml` の `NATS_SERVER_VERSION` を読み、その版の
broker を `target/magi-tools/nats/<version>/<platform>/bin` に準備する。
一致する PATH 上の既存バイナリをコピーするか、GitHub の公式リリースから
固定版を取得する。グローバルにインストールされた別の版は変更しない。
キャッシュは版・OS・arch ごとに分離し、子テストの PATH の先頭にだけ追加する。
既存キャッシュも取得直後のバイナリも実行版を照合し、不一致は失敗にする。
取得失敗、未対応ホストも失敗であり、テストのスキップには置き換えない。

ネットワークが制限された環境では、事前取得した同版のバイナリを指定する。
指定ファイルも固定版との一致が必須で、不正なら別の取得先へフォールバックしない。

```sh
KANADE_TEST_NATS_SERVER=/path/to/pinned/nats-server python3 scripts/ci/magi-e2e.py
```

初回準備後は、通常の `python3 scripts/ci/magi-e2e.py` がキャッシュを使う。
loopback の bind/connect 権限は引き続き必要。
各コマンドの `running N tests` と
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

## 修正後の discovery とレイヤー合成の確認

review round 1 では `magi 0.173.0` の `magi doctor` を実行した。
このコマンドは fetch や run 開始をせず、`Config::discover` で解決した
レイヤーと実効 `verify` を表示する。通常の
`magi doctor --repo . --no-color` は終了 0 だったが、抽出元は
launcher 固定 base `65f6be313c214133a8ec0849bc24aeee158e0712` であり、
まだ `cargo test --doc` と旧 e2e 2 コマンドを表示した。
親 run の保存済み設定が旧設定であることとも矛盾しない。

修正候補の抽出元は `9cbbc1c5c14e8cfb7e2d3bcb36f99a2459acb571`。
固定 base を祖先に持つことを検査したうえで、次のコマンドで検証した。
一時 bare Git 管理領域の `refs/remotes/origin/main` のみを候補コミットに
設定し、既存オブジェクトを読み取り専用で参照する。追加の checkout、
worktree、ローカル branch は作らず、実際の共有 ref、機械設定、保存済み
run は変更しない。`--config` や `MAGI_CONFIG_DIR` の上書きも使わない。
これは候補が上流に入った場合の抽出経路の検証であり、実際の upstream
が更新されたという主張ではない。

```sh
python3 - <<'PY'
import os
from pathlib import Path
import subprocess
import tempfile

repo = Path.cwd()
def git(*args, env=None):
    return subprocess.check_output(
        ['git', *args], cwd=repo, env=env, text=True).strip()

base = '65f6be313c214133a8ec0849bc24aeee158e0712'
commit = '9cbbc1c5c14e8cfb7e2d3bcb36f99a2459acb571'
subprocess.run(['git', 'merge-base', '--is-ancestor', base, commit], check=True)
objects = Path(git('rev-parse', '--path-format=absolute',
                   '--git-common-dir')) / 'objects'
with tempfile.TemporaryDirectory(prefix='kanade-discovery-') as tmp:
    admin = Path(tmp) / 'git'
    subprocess.run(['git', 'init', '--bare', str(admin)],
                   check=True, capture_output=True)
    (admin / 'objects/info/alternates').write_text(str(objects) + '\n')
    env = dict(os.environ, GIT_DIR=str(admin), GIT_WORK_TREE=str(repo))
    git('update-ref', 'refs/remotes/origin/main', commit, env=env)
    git('symbolic-ref', 'refs/remotes/origin/HEAD',
        'refs/remotes/origin/main', env=env)
    result = subprocess.run(
        ['magi', 'doctor', '--repo', str(repo), '--no-color'],
        env=env, text=True, capture_output=True)
    print(result.stdout)
    print(result.stderr)
    print('doctor exit:', result.returncode)
    assert result.returncode == 0
    layers = list((admin / 'magi/config').rglob('magi.toml'))
    assert len(layers) == 1
    assert str(layers[0]) in result.stdout
    assert layers[0].read_bytes() == subprocess.check_output(
        ['git', 'show', f'{commit}:magi.toml'])
PY
```

終了 0。読み込まれたレイヤーの順序は、実際の機械設定
`magi/config.toml` → 一時 Git 管理領域の
`magi/config/9cbbc1c5c14e8cfb7e2d3bcb36f99a2459acb571-<repo-path-hash>/magi.toml`
だった。候補 tree に `.magi/config.toml` はなく、`magi.toml` に
`include` 宣言もないため、追加レイヤーはない。抽出された設定の bytes は
`git show <候補コミット>:magi.toml` と一致した。以下は doctor の出力から
実効設定部分をそのまま抜粋したもの。

```text
verify.e2e   python3 scripts/ci/magi-e2e.py
verify.gate  cargo check --all-targets && cargo test --all-targets && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo check --locked --all-targets
```

したがって修正設定は cwd からの直接読み込みだけでなく、現行 magi の
remote-tracking ref → コミット抽出 → 機械/repo レイヤー合成で利用できる。
この経路で設定が無視される上流 magi 不具合は再現しなかった。
既存 run の有効設定はこの確認では書き換わらず、実際の base に候補が
含まれるまでは通常の discovery は旧設定を返す。
doctor はコマンドの発見・合成の検証であり、gate/e2e の成功を示さない。
テスト実行結果と NATS の blocker は下記のとおりで、全体検証と最終 gate
の再実行は親 run が所有する。

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
