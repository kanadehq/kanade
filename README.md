# kanade

> 奏 — *orchestrate*. Windows 端末を数千台規模で一元管理する
> Rust 製の pub/sub 基盤。NATS / JetStream を中核に、インベントリ採取・
> 全体配信・緊急コマンドを 1 つのチャネルに乗せる。

**Status: 0.1.0 — Sprint 1 (PoC) 構築中。** Agent + Admin CLI + ローカル NATS
単体での疎通までを最初の単位とする。設計の全体像は [docs/SPEC.md](./docs/SPEC.md)
を参照。

## 構成

| crate            | 種別 | 役割 |
|------------------|------|------|
| `kanade-shared`  | lib  | 通信用の型 (`Command` / `ExecResult` / `Heartbeat`) と teravars ベースの設定ローダ |
| `kanade-agent`   | bin  | Windows 端末側の常駐デーモン。NATS 接続 → `commands.*` を subscribe → 子プロセス実行 → `ExecResult` を publish。30 秒ごとに heartbeat も publish |
| `kanade`         | bin  | オペレーター側の管理 CLI。NATS 越しに request/reply でコマンドを送る |

## ローカルで動かす

NATS server をローカルで起動:

```powershell
scoop install nats-server   # winget install nats-io.nats-server でも可
nats-server -js -p 4222
```

Agent を別ターミナルで起動 (リポジトリ直下の `agent.toml` を使う):

```powershell
cargo run -p kanade-agent
```

CLI から疎通 (`$env:COMPUTERNAME` がそのまま pc_id になる):

```powershell
cargo run -p kanade -- run $env:COMPUTERNAME -- 'echo hello from kanade'
```

`stdout` 欄に `hello from kanade` が出れば OK。

heartbeat の生存確認:

```powershell
cargo run -p kanade -- ping $env:COMPUTERNAME
```

## 設定ファイル (`agent.toml`)

Tera テンプレートを含む TOML を [teravars] crate で展開する。Windows / Linux
両 OS で同じ 1 ファイルが動作する設計 ([SPEC.md §2.4.4](./docs/SPEC.md) 参照)。

## 開発タスク

```powershell
cargo make check       # fmt-check + clippy + test + lock-check
cargo make fmt         # フォーマット適用
cargo make on-add      # renri の post_create hook (apm install + vcs fetch)
```

## Sprint 1 スコープ

- [x] Cargo workspace + 3 crate (`shared` / `agent` / CLI)
- [x] Agent: NATS 接続、`commands.all` + `commands.pc.{pc_id}` + `kill.>` subscribe
- [x] Agent: 子プロセス実行 → `ExecResult` を `results.{request_id}` に publish
- [x] Agent: 30 秒ごとに `heartbeat.{pc_id}` を publish
- [x] CLI: `run` (request/reply) と `ping` (heartbeat wait)
- [ ] `cargo make check` の workspace 対応確認
- [ ] ローカル NATS で echo 疎通テスト

Sprint 2 以降: Windows Service 化 (`windows-service` crate)、インベントリ採取
(WMI)、`kill.>` での子プロセス強制終了、YAML ジョブ定義パーサ、Wave / Jitter、
mTLS、自己アップデート、Backend (axum + SQLite + Projector) — [docs/SPEC.md](./docs/SPEC.md)
の Sprint 2〜6 を参照。

## kata preset

このリポジトリのスケルトン (`AGENTS.md` / `Makefile.toml` / `clippy.toml` /
`rustfmt.toml` / `.github/workflows/*` / etc.) は
[`github.com/yukimemi/pj-presets:rust-cli`](https://github.com/yukimemi/pj-presets)
preset から `kata init` で投入したものをベースにしている。`Cargo.toml` の
workspace 構造とローカル `Cargo.toml` 群は手動で書いた (preset 側で workspace
サポートが入るまでの暫定)。

[teravars]: https://github.com/yukimemi/teravars
