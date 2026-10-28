# 端末管理システム 仕様書

**対象**: Windows PC 数千台規模の一元管理基盤
**設計方針**: pub/sub + 永続化ストリーミングによる軽量分散管理
**言語**: 全コンポーネント Rust
**最終更新**: 2026-05-15

---

## 目次

- [Part 1: 概要設計](#part-1-概要設計)
  - [1.1 システム概要](#11-システム概要)
  - [1.2 主要ユースケース](#12-主要ユースケース)
  - [1.3 設計方針](#13-設計方針)
  - [1.4 アーキテクチャ全体像](#14-アーキテクチャ全体像)
  - [1.5 技術スタック](#15-技術スタック)
  - [1.6 段階的構築計画](#16-段階的構築計画)
- [Part 2: 詳細設計](#part-2-詳細設計)
  - [2.1 コンポーネント仕様](#21-コンポーネント仕様)
  - [2.2 NATS Subject 設計](#22-nats-subject-設計)
  - [2.3 データ設計](#23-データ設計)
  - [2.4 命令定義 (YAML スキーマ)](#24-命令定義-yaml-スキーマ)
  - [2.5 配信戦略](#25-配信戦略)
  - [2.6 バージョン管理と緊急停止 (3層防御 + オフライン補強)](#26-バージョン管理と緊急停止-3層防御--オフライン補強)
  - [2.7 セキュリティ](#27-セキュリティ)
  - [2.8 信頼性・可用性](#28-信頼性可用性)
  - [2.9 監視・運用](#29-監視運用)
  - [2.10 デプロイ構成とサイジング](#210-デプロイ構成とサイジング)
  - [2.11 リポジトリ・ディレクトリ構成](#211-リポジトリディレクトリ構成)
  - [2.12 KLP (Kanade Local Protocol)](#212-klp-kanade-local-protocol)
  - [2.13 クロスプラットフォーム デプロイ設計 (kanade deploy CLI & Typed Operations)](#213-クロスプラットフォーム-デプロイ設計-kanade-deploy-cli--typed-operations)

---

# Part 1: 概要設計

## 1.1 システム概要

数千台規模の Windows PC を、AD 非依存で一元管理する自前基盤。
インベントリ採取・全体配信・緊急コマンド実行の 3 用途を、単一の pub/sub 基盤でカバーする。

商用 (Intune, Tanium 等) を導入せず自作する場合の参照アーキテクチャ。

## 1.2 主要ユースケース

| # | 種別 | 例 | 性質 |
|---|------|----|----|
| ① | 定期情報採取 (インベントリ) | 電源 ON/OFF、サインイン、HW/SW 情報、ネットワーク・ドライバ情報 | 周期的 + イベント駆動 |
| ② | 定期全体配信 | パッチ適用、SW アップデート、ファイル配信、設定変更 | 計画的、Wave 配信 |
| ③ | 一時的な緊急処置 | 特定端末への任意コマンド、緊急情報採取 | 同期 Request-Reply |

## 1.3 設計方針

1. **pub/sub + 永続化ストリーミング**
   ファイルベースやクラサバ (Server→Client push) ではなく、エージェント発の outbound 接続 + メッセージブローカー方式を採用。FW/NAT フレンドリーであり、ファンアウト・ファンインを標準機能としてサポートします。

2. **AD 非依存 (mTLS で認証完結)**
   AD 参加環境でも動くが、AD を前提条件にしない。クライアント証明書で認証する。

3. **設定駆動 (YAML + Git)**
   命令はコード化せず宣言的に YAML で記述し、Git で管理。レビュー・履歴・ロールバックを Git に乗せる (GitOps)。

4. **イベントソーシング / CQRS**
   JetStream Stream を Source of Truth とし、SQLite はクエリ用の投影 (Projection) として位置づける。SQLite が壊れても Stream から再構築可能。

5. **段階的構築 (1 台 → HA)**
   PoC は 1 サーバーで開始でき、規模拡大に応じて NATS クラスタ化・Backend 冗長化を後付けできる。

6. **Rust スタック統一**
   Agent / Backend / CLI を全て Rust で実装。共有 crate で型を統一し、API ミスマッチを排除。

## 1.4 アーキテクチャ全体像

```
┌──────────────────────────────────────────────────────────┐
│                       オペレーター                          │
└────────────┬─────────────┬──────────────┬─────────────────┘
             │             │              │
       [Admin CLI]   [Web UI (SPA)]  [Slack Bot / CI]
             │             │              │
             └─────────────┼──────────────┘
                           │  HTTPS API (axum)
                           ▼
                  ┌──────────────────┐
                  │     Backend      │
                  │ ┌──────────────┐ │
                  │ │ API (axum)   │ │
                  │ │ Scheduler    │ │
                  │ │ Projectors   │ │ ─────> [SQLite]
                  │ │ Workers      │ │       (Projection)
                  │ └──────────────┘ │
                  └────────┬─────────┘
                           │
                           ▼
                  ┌──────────────────────────────┐
                  │ NATS + JetStream             │
                  │  ├─ Streams (events / audit) │
                  │  ├─ KV (state / config)      │
                  │  └─ Object Store (files)     │
                  └────────┬─────────────────────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
          [Agent #1]  [Agent #2] ...[Agent #3000]
        (Windows Service / Rust 1 binary)
```

## 1.5 技術スタック

### サーバー側

| レイヤー | 採用技術 | 備考 |
|---|---|---|
| OS | **Linux** (Ubuntu Server / Rocky Linux) **または Windows Server** | systemd / Windows Service の両対応 |
| メッセージ基盤 | NATS + JetStream | 単一バイナリ、Win/Linux 両対応 |
| Backend 言語 | Rust + Tokio | async-nats / axum |
| Web フレームワーク | axum | API + SPA 静的配信を同居 |
| DB | SQLite (`sqlx`) | 投影ストア。スケール時 Postgres へ |
| フロントエンド | React/Vue/Svelte + TypeScript | `rust-embed` でバイナリに焼き込み |
| 型共有 | `ts-rs` または `specta` | Rust → TS 自動生成 |
| スケジューラ | `tokio-cron-scheduler` | operator 表面は `when`（§2.4.3）、内部で cron に lower |
| 設定読み込み | **TOML + `yukimemi/teravars`** | `[vars]` 自己参照 + `is_windows()` で OS 分岐 |
| ロギング | `tracing` + `tracing-subscriber` | Windows ではイベントログ併用 |

**OS 選定の指針**:
- 既存運用が Windows Server 中心なら、**Windows Server** で問題ありません。NATS、Rust、SQLite のすべてが Windows 上で完全に動作します。
- Linux のほうが NATS / Rust エコシステムの運用情報が豊富
- 両 OS で同一バイナリが動くよう、Rust コードはパス区切り・パーミッション周りを抽象化 (`std::path::PathBuf`, `dirs` crate 等)

### Agent 側 (Windows PC)

| レイヤー | 採用技術 |
|---|---|
| OS | Windows 10/11 |
| サービス化 | `windows-service` crate (LocalSystem) |
| 言語 | Rust + Tokio |
| NATS クライアント | `async-nats` |
| ファイル監視 | `notify` + `notify-debouncer-full` |
| シリアライゼーション | `serde` + `serde_json` |
| ローカル永続化 | SQLite (`rusqlite`) または JSON ファイル |
| 設定 | **TOML + `yukimemi/teravars`** (spyrun と同パターン) |
| ロギング | `tracing` + Windows イベントログ |

## 1.6 段階的構築計画

| Phase | 構成 | 用途 |
|---|---|---|
| **Phase 1** | Agent + Admin CLI + NATS (1 ノード) | PoC、〜数百台 |
| **Phase 2** | Phase 1 + Backend (axum + SPA + SQLite + Projector) | チーム運用、〜数千台 |
| **Phase 3** | NATS 3 ノードクラスタ + Backend 冗長化 + Postgres + 外部 LB | 商用品質、HA 必須 |

PoC を Phase 1 で立ち上げ、運用が軌道に乗ってから Backend を追加するのが現実的です。NATS のメッセージ契約 (Subject 設計) を最初から後方互換に保つことで、後からの追加による破壊的変更を避けます。

---

# Part 2: 詳細設計

## 2.1 コンポーネント仕様

### 2.1.1 Agent

**役割**: Windows PC 上で常駐し、インベントリ採取・コマンド受信・実行・結果報告を行う。

**起動形態**: Windows Service (`MgmtAgent`)、`LocalSystem` アカウント

**主な責務**:
- NATS 接続維持 (自動再接続)
- 自分宛/グループ宛/全体宛 Subject の subscribe
- スクリプト/コマンドの実行 (子プロセス管理)
- 結果・状態・イベントの publish
- 定期インベントリ採取 (内部スケジューラ)
- 自己アップデート (Object Store からバイナリ取得)
- kill signal の監視

**内部構成**:

```rust
// main.rs (簡略)
service_dispatcher::start("MgmtAgent", ffi_service_main)?;

fn service_main(_args: Vec<OsString>) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let nats = async_nats::connect("nats://server:4222").await?;
        tokio::join!(
            command_subscriber(nats.clone()),
            inventory_scheduler(nats.clone()),
            event_publisher(nats.clone()),
            heartbeat_publisher(nats.clone()),
            kv_config_watcher(nats.clone()),
        );
    });
}
```

**設定ファイル**: `C:\ProgramData\Mgmt\agent.toml` (TOML)
**ログ出力先**: `C:\ProgramData\Mgmt\logs\*.log` + Windows イベントログ
**バイナリ配置**: `C:\Program Files\Mgmt\agent.exe`

### 2.1.2 Backend

**役割**: 運用ガバナンスの集約点。CLI / Web UI / Bot からの要求を受け、認証・検証・監査・NATS への publish を行う。

**主な責務**:
- HTTPS API 提供 (axum)
- Web UI 用静的ファイル配信 (`rust-embed`)
- 認証・認可 (OIDC / LDAP)
- YAML 命令の検証・パース・NATS への変換
- 監査ログ記録
- スケジューラ (定刻トリガで NATS publish)
- Projector (Stream → SQLite 投影)
- Worker (結果回収・後処理)

**内部構成** (単一プロセス内の論理構成):

```
[axum HTTP Server]
   ├─ /api/* ─── REST API
   └─ /*     ─── SPA (rust-embed)

[Scheduler]      ── cron トリガで publish
[Projector × N]  ── Stream subscribe → SQLite 書き込み
[Result Worker]  ── results.> を queue group で並列処理
[Audit Worker]   ── audit イベントを SQLite に記録
```

**起動形態**: Linux なら systemd service、Windows Server なら Windows Service として常駐 (どちらも同じバイナリ)
**設定ファイル**: `/etc/mgmt/backend.toml` (Linux) または `C:\ProgramData\Mgmt\backend.toml` (Windows) — TOML + Tera 変数展開
**ログ**: `journalctl` / `/var/log/mgmt/*.log` (Linux) または Windows イベントログ + `C:\ProgramData\Mgmt\logs\*.log` (Windows)

### 2.1.3 Admin CLI (`mgmtctl`)

**役割**: オペレーターの手元で動くツール。YAML 読み込み、Backend API 呼び出し、進捗表示。

**主なサブコマンド**:

```
mgmtctl deploy <yaml>           # 配信実行 (--dry-run / --approve)
mgmtctl status <deploy_id>      # 配信進捗確認
mgmtctl agents list             # 端末一覧
mgmtctl agents inspect <pc_id>  # 個別端末の詳細
mgmtctl run <pc_id> -- <cmd>    # アドホックコマンド (request-reply)
mgmtctl kill <job_id>           # 実行中ジョブの停止
mgmtctl logs <deploy_id>        # 結果ログ取得
```

**認証**: OIDC デバイスフローでトークン取得、`~/.mgmtctl/token` に保存

### 2.1.4 Web UI

**実装**: React/Vue/Svelte + TypeScript (任意)
**配信**: Backend の axum が `rust-embed` で同梱・配信 (単一バイナリ)
**主要画面**:
- ダッシュボード (端末数、配信状況、アラート)
- 端末一覧 (検索・フィルタ)
- 端末詳細 (インベントリ、履歴)
- 配信管理 (進行中・履歴)
- 監査ログ
- スケジュール管理

**型共有**: `ts-rs` で Rust の API 型から TS 型を自動生成

### 2.1.5 Client App (`kanade-client`)

**役割**: エンドユーザー (Windows PC 利用者、**管理者権限なし**) が自分の端末状態を把握し、管理者からの通知を受け、許可された範囲のソフトウェア更新・トラブルシューティングを自分で実行するための GUI フロントエンド。

特権操作 (Office 修復、サービス再起動、レジストリ書き換え等) はすべて `LocalSystem` で動く Agent 側で実行され、Client App は UI 入力と進捗表示のみを担当する。これにより、「**管理者権限を持たないエンドユーザーが自力で復旧操作を完結できる仕組み**」を実現します。

**起動形態**: ユーザーセッション上の常駐プロセス。
- タスクトレイに常駐し、必要なときにウィンドウを開く
- ログオン時自動起動 (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 経由)
- MSI でのマシン全体インストール時に **ActiveSetup** で全ユーザー初回ログオン時に自動展開

**実装**: **Tauri 2.x** (Rust backend + WebView2 frontend)
- フロント: 既存 SPA (`crates/kanade-backend/web`) と同じ TS スタックを再利用、共通コンポーネントは別 crate / npm workspace に切り出して共有
- Rust 側: `kanade-shared` 経由で型共有 (KLP の各 method 型を ts-rs export して TS から `import type` で参照)

**主な責務**:

1. **通知 (Notification) 表示**
   - 起動時に未読通知があれば自動表示
   - 過去メッセージ一覧 (既読/未読)、検索・フィルタ
   - `priority: emergency` はモーダル + 「確認」 ボタン強制 (`require_ack: true`)
   - 確認操作を Agent → NATS `events.notifications.acked.{pc_id}.{notif_id}` で SPA に流す ⇒ SPA 側で「該当ユーザーが何時何分に確認したか」 を追える

2. **端末ヘルス・状態表示**
   - フリート疎通状態 (`online`) をリアルタイム表示。VPN 接続状態などサイト個別の状態は専用フィールドを設けず、operator 定義の `check:` ジョブ (例: `check-vpn.yaml`) として下記コンプライアンスチェックに並べる
   - インストール済ソフトウェアのバージョン (最新? 更新待ち?)
   - 「**コンプライアンスチェック**」 として 5〜10 項目を ✅/⚠️/❌ 表示:
     BitLocker 有効、AV 最新、OS パッチ最新、証明書期限 30 日超、ディスク空き 10% 超、Agent self-update 完了、等
   - NG 項目には「修復する」 ボタン (該当するトラブルシューティングジョブを起動)

3. **ソフトウェアアップデート (セルフサービス)**
   - Manifest で `user_invokable: true` かつ `category: software_update` であるジョブを「アップデート」 タブに表示
   - ユーザーがクリック → KLP `jobs.execute` → Agent IPC 経由で `commands.pc.{pc_id}` publish → 実行は `LocalSystem` の Agent が行う
   - 進捗バー + 完了通知 + SPA への結果反映 (既存 `results.>` 経路)

4. **トラブルシューティング**
   - `category: troubleshoot` であるジョブを「困ったとき」 タブに表示 (Teams キャッシュクリア、Office 修復、ネットワークアダプタ再起動 等)
   - 同上の KLP 経由実行。`run_as: user` の項目はユーザーセッション側で、`run_as: system` の項目は LocalSystem で実行

5. **サポート連絡 + 診断ログ収集**
   - 「サポートに問い合わせる」 ボタン → KLP `support.upload_diagnostics` → Agent が `{pc_id, recent_inventory, last_N_events, agent_log_tail}` を Object Store にアップロード + チケット起票
   - ヘルプデスクの初動が迅速化

6. **メンテナンス予約・延期申請**
   - 自分の端末に予定された job 一覧 (今後 N 日)
   - 配信された再起動通知の延期申請 (15分 / 30分 / 1時間)

**追加候補機能** (将来):
- パスワード期限通知 (AD 連携 or Windows API)
- VPN / プロキシのワンクリック再接続
- フィッシング報告ボタン (`events.security.report.{pc_id}` 起票)
- セルフサービスソフトウェアカタログ (`category: catalog`)
- 言語切替 (ja/en) + ダーク/ライトテーマ

**設計原則**:

- **NATS には直接繋がない**。NATS 認証情報 (mTLS 証明書) は LocalSystem Agent が独占管理し、Client は **KLP (§2.12)** 経由でのみ Agent と話す。
- **特権操作は全て Agent 側**。Client は UI とユーザー入力検証のみ。
- **OS 認証を信用する**。Client が payload に user_id を埋めても Agent は無視し、IPC 接続元 token から取った SID を使う。
- **マルチユーザー対応**。Fast User Switching / RDP で複数ユーザーが同時にログオンしている場合、Agent はセッションごとにクライアントの接続を識別し、通知はセッション単位でファンアウト（配信）されます。

**設定ファイル**: `%APPDATA%\Kanade\client.toml` (ユーザー別)
**ログ出力先**: `%LOCALAPPDATA%\Kanade\logs\client.log`
**バイナリ配置**: `C:\Program Files\Kanade\kanade-client.exe`

## 2.2 NATS Subject 設計

### 2.2.1 配信系 (Backend → Agent)

| Subject | 用途 |
|---|---|
| `commands.all` | 全台向けコマンド |
| `commands.group.{group_name}` | グループ単位コマンド (canary, wave1 等) |
| `commands.pc.{pc_id}` | 個別端末向けコマンド |
| `commands.deploy.{job_id}` | 配信ジョブ (バージョン管理対象) |
| `notifications.all` | 全台向け通知 (エンドユーザー向け、Client App で表示) |
| `notifications.group.{group_name}` | グループ単位通知 |
| `notifications.pc.{pc_id}` | 個別端末向け通知 |

### 2.2.2 報告系 (Agent → Backend)

| Subject | 用途 |
|---|---|
| `inventory.{pc_id}.{category}` | インベントリ (category: hw, sw, net, driver 等) |
| `events.{pc_id}.{type}` | リアルタイムイベント (power.on, session.signin 等) |
| `events.notifications.acked.{pc_id}.{user_sid}.{notif_id}` | ユーザーが Client App で通知を確認した記録 (`{user_sid}` で同 PC 複数ユーザーを識別) |
| `events.notifications.dismissed.{pc_id}.{user_sid}.{notif_id}` | ユーザーが通知を閉じた記録 (require_ack=false 時) |
| `results.{request_id}` | コマンド実行結果 |
| `heartbeat.{pc_id}` | 死活確認 (定期) |

### 2.2.3 制御系

| Subject | 用途 |
|---|---|
| `kill.{exec_id}` | 特定 exec (1 発火/デプロイ) の実行中プロセス即時停止 |
| `kill.all` | 全ジョブ即時停止 (緊急時) |
| `config.update` | 設定変更通知 |

### 2.2.4 ワイルドカード購読パターン

Backend は以下のワイルドカードで一括購読する:

```
inventory.>     # 全インベントリ
events.>        # 全イベント
results.>       # 全結果
heartbeat.>     # 全死活
```

## 2.3 データ設計

### 2.3.1 JetStream Stream (時系列・追記専用)

| Stream 名 | 対象 Subject | 用途 | 保持 |
|---|---|---|---|
| `INVENTORY` | `inventory.>` | インベントリ履歴 | 90 日 |
| `EVENTS` | `events.>` | リアルタイムイベント履歴 | 30 日 |
| `RESULTS` | `results.>` | コマンド実行結果履歴 | 30 日 |
| `AUDIT` | `audit.>` | 監査ログ (Backend が publish) | 永続 |
| `DEPLOY` | `commands.deploy.>` | 配信ジョブ (MaxMsgsPerSubject=1) | 7 日 |
| `NOTIFICATIONS` | `notifications.>` | エンドユーザー向け通知履歴 (Client App 過去メッセージ表示用) | 90 日 |

`DEPLOY` のみ `MaxMsgsPerSubject=1` + `DiscardPolicy::Old` で「同一 job_id の最新版のみ保持」を実現する。`NOTIFICATIONS` は履歴として全件残し、Client App が起動時に未読分を fetch する。

### 2.3.2 JetStream KV (現在状態)

| KV Bucket | キー | 値 | 用途 |
|---|---|---|---|
| `agents_state` | `{pc_id}` | JSON (latest inventory) | 端末の最新状態 (`history=1`) |
| `agent_groups` | `{pc_id}` | JSON `{"groups":[...]}` (Sprint 5) | この PC が属するグループ集合。Agent が watch して `commands.group.<name>` 購読を動的に追加/解除 |
| `agent_config` | `global` / `groups.<name>` / `pcs.<pc_id>` (Sprint 6) | JSON (`ConfigScope`、partial) | Fleet 全体 / グループ / PC 単位の重ね合わせ設定。詳細は §2.3.5 |
| `script_current` | `{cmd_id}` | バージョン文字列 | 現行有効バージョン |
| `script_status` | `{cmd_id}` | `"ACTIVE"` / `"REVOKED"` | 緊急停止フラグ |
| `schedules` | `{schedule_id}` | JSON (when, target, active) | スケジュール定義 (`kanade schedule create` → backend HTTP → このバケット) |
| `notifications_read` | `{pc_id}.{user_sid}.{notification_id}` | JSON (`{"acked_at": ..., "acked_by": "<sid>"}`) | エンドユーザー既読状態 (per-user)。Agent が KLP `notifications.ack` を受けて接続元 SID 付きで書き込み、SPA から確認状況を参照。`{pc_id}.{user_sid}.` プレフィクスで該当ユーザーの既読一覧を効率取得 |

NATS KV のバケット名は domain-safe ASCII (英数 + `_-`) のみで `.` 不可。仕様初期に書いた `script.current` 等は実装では underscore form (`script_current`) に正規化されている。配送ジョブ進捗 (`deployments`) は SQLite に projection するので KV ではなく Stream + projector 経由 (§2.3.4)。

### 2.3.3 JetStream Object Store

| Bucket 名 | 用途 |
|---|---|
| `installers` | パッチ・インストーラ等の大容量ファイル |
| `scripts` | 大きめのスクリプト (>数MB の場合) |
| `agent_releases` | Agent 自身のアップデートバイナリ |

### 2.3.4 SQLite (投影 / Projection)

Stream を Projector worker が消費し、以下のテーブルに投影する。

```sql
-- 端末マスター + 最新状態 (検索用)
CREATE TABLE agents (
    pc_id TEXT PRIMARY KEY,
    hostname TEXT,
    os_version TEXT,
    last_seen TIMESTAMP,
    last_signin_user TEXT,
    is_online BOOLEAN,
    -- 検索しやすい属性を抜き出して列に
    updated_at TIMESTAMP
);

-- 配信履歴 (検索可能形)
CREATE TABLE deployments (
    deploy_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    version TEXT NOT NULL,
    initiated_by TEXT NOT NULL,
    initiated_at TIMESTAMP NOT NULL,
    target_count INTEGER,
    success_count INTEGER,
    failure_count INTEGER,
    status TEXT  -- pending / running / completed / failed / cancelled
);

CREATE TABLE deployment_results (
    deploy_id TEXT,
    pc_id TEXT,
    status TEXT,  -- success / failure / timeout / skipped
    exit_code INTEGER,
    stdout TEXT,
    stderr TEXT,
    executed_at TIMESTAMP,
    PRIMARY KEY (deploy_id, pc_id)
);

-- 監査ログ (検索用)
CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    payload JSON,
    occurred_at TIMESTAMP NOT NULL
);

CREATE INDEX idx_audit_actor ON audit_log(actor, occurred_at);
CREATE INDEX idx_audit_action ON audit_log(action, occurred_at);

-- ユーザ・ロール (RBAC)
CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT, email TEXT);
CREATE TABLE roles (user_id TEXT, role TEXT, PRIMARY KEY(user_id, role));
```

**重要原則**: SQLite は再構築可能なキャッシュ。破損時は JetStream Stream の replay で復旧する。

### 2.3.5 層化された agent_config (Sprint 6)

`agent_config` バケットは **3 層 + ビルトイン default** の重ね合わせ。Agent 起動時の `config_supervisor` タスクが両バケット (`agent_config` + `agent_groups`) を watch し、変更を受けるたびに resolver でフラット化、`tokio::sync::watch` チャネルで heartbeat / inventory / self_update に配布する。

```
ビルトイン default (compiled-in)            ← 何も設定しなければ常にこの値
        ↓
agent_config:global                          ← Fleet 全体の default
        ↓
agent_config:groups.<name>                   ← 当該 PC が属する全グループの override が
                                              アルファベット順に重ね合わせ (last wins)
        ↓
agent_config:pcs.<pc_id>                     ← この PC 専用の override (最優先)
        ↓
= EffectiveConfig (Agent が実際に走る値)
```

`ConfigScope` の各フィールドは `Option<T>`。`Some` = この層で値を設定、`None` = 下の層に委譲。同一フィールドを複数のグループが設定している場合、警告 (`ResolutionWarning::MultiGroupConflict`) が emit され、アルファベット順最後のグループの値が採用される。

サポートフィールド (Sprint 6 時点):
- `target_version` — self-update 発火条件 (層化対応により canary rollout が可能)
- `inventory_interval` / `inventory_jitter` / `inventory_enabled`
- `heartbeat_interval`

操作:
- `kanade config get/set/unset/clear/effective [--group <n>|--pc <pc_id>]` — backend HTTP 経由 (`KANADE_AUTH_TOKEN`、変更は operator 以上。NATS 直結ではない)
- `GET/PUT/DELETE /api/config`, `/api/groups/{n}/config`, `/api/pcs/{p}/config` — backend HTTP (スコープ全体の置換 / 削除)
- `PUT/DELETE /api/config/fields/{field}`, `/api/groups/{n}/config/fields/{field}`, `/api/pcs/{p}/config/fields/{field}` — 1 フィールドの set / unset (サーバー側 CAS read-modify-write。変化が無ければ書き込まない)
- `GET /api/agents/{pc_id}/effective_config` — 解決済み view (debug 用)

## 2.4 命令定義 (YAML スキーマ)

### 2.4.1 ジョブ定義 (jobs/*.yaml)

```yaml
id: cleanup-disk-temp           # 必須、job id / cmd_id として使用
version: 1.0.1                  # 必須、semver
description: "Temp ディレクトリのクリーンアップ"

execute:
  shell: powershell             # powershell / cmd / sh / pwsh
  # スクリプト本体: script / script_file / script_object のうち
  # **ちょうど 1 つ**を指定。複数指定や全省略は `kanade job create`
  # / `POST /api/jobs` の parse 時点で 400 リジェクト。空文字列
  # (`script: ""`) は「未設定」と等価に扱う (block scalar をコメ
  # ントアウトしただけの操作を許容する) 。
  script: |                     # インライン (small)
    $temp = [System.IO.Path]::GetTempPath()
    Remove-Item "$temp\*" -Recurse -Force -ErrorAction SilentlyContinue
    Write-Output "cleaned: $temp"
  # script_file: scripts/cleanup.ps1                   # repo-local file (CLI が読んで script に差し込む)
  # script_object: cleanup-disk-temp/1.0.1            # OBJECT_SCRIPTS の `<name>/<version>` キー
  timeout: 600s
  run_as: system                # system / user

require_approval: true          # 本番配信時に承認必須 (省略時 false)
tags: [maintenance, disk]       # オペレーター分類用タグ (省略可)

# --- エンドユーザー Client App カタログ公開 (オプトイン) ---
client:
  user_invokable: true          # true なら Client App に表示
  category: troubleshoot        # software_update / troubleshoot / catalog
  display_name: "Temp フォルダのクリーンアップ"  # Client App での表示名 (省略時は id)
  display_description: "..."    # Client App でのツールチップ
  icon: "broom"                 # Client App でのアイコン名 (任意)

# --- オプトインヒント・拡張属性一覧 (Manifest の全オプトイン機能) ---
# inventory:                    # インベントリ事実収集 (JSON Facts)
# check:                        # ヘルスチェック状態更新
# collect:                      # ファイル収集アーカイブログ作成
# emit:                         # per-line イベントログ送出 (NDJSON)
# aggregate:                    # Analytics ダッシュボード集計定義 (obs_events)
# feed:                         # 外部参照データフィード定義 (Controller 階層)
# finalize:                     # ジョブ実行完了後の後処理フック
# staleness:                    # キャッシュ / オフライン実行ポリシー (§2.6.2)
# origin:                       # GitOps / 定義元プロベナンス情報
# tier:                         # 実行対象層 (endpoint | controller)
```

> **Note (v0.18.0+)**: 配信対象 (`target`), 段階的配信 (`rollout`), 再試行 (`on_failure`), ジッター (`jitter`) は Schedule 定義 (§2.4.3) 側に移動しました。Job Manifest はスクリプト本体とオプトインの実行属性のみを所有します。

#### Manifest フィールドリファレンス

| フィールド | 型 | 概要 |
|---|---|---|
| `id` | `String` (必須) | ジョブの一意な識別子 |
| `version` | `String` (必須) | バージョン表記 (semver) |
| `description` | `String` | ジョブの概要・説明テキスト |
| `execute` | `Execute` (必須) | スクリプト実行仕様 (`shell`, `script`/`script_file`/`script_object`, `timeout`, `run_as` 等) |
| `require_approval` | `bool` | 本番配信・実行時の手動承認の必須化 (デフォルト: `false`) |
| `tags` | `Vec<String>` | オペレーター分類・フィルタリング用タグのリスト |
| `client` | `ClientHint` | エンドユーザー Client App (KLP) へのカタログ公開設定 |
| `inventory` | `InventoryHint` | stdout から JSON Facts を収集し `inventory_facts` へ挿入 |
| `check` | `CheckHint` | Client App の Health タブへ反映するヘルスチェック状態を収集 |
| `collect` | `CollectHint` | スクリプトで収集したファイルの zip アーカイブ作成・Object Store 送出 |
| `emit` | `EmitConfig` | stdout の 1 行ごとに `ObsEvent` NDJSON を `obs_events` テーブルへストリーミング送出 |
| `aggregate` | `Vec<AggregateWidget>` | `obs_events` に対する Analytics ページ向けの宣言的集計定義 |
| `feed` | `Vec<FeedSpec>` | 外部参照データ (CVE、EOL テーブル等) を Shared `feeds` テーブルへ反映 (Controller 階層) |
| `finalize` | `FinalizeSpec` | メインスクリプト正常終了後に実行する後処理フック |
| `staleness` | `Staleness` | オフライン/キャッシュ実行時の厳格度・ポリシー設定 (§2.6.2) |
| `origin` | `RepoOrigin` | GitOps リポジトリ等、構成管理上の定義元情報 |
| `tier` | `Tier` | 実行階層の制約 (`endpoint` / `controller`) |

**`client` ブロック**:
- 省略時 (= `None`) は operator 起動専用のジョブとなります
- `client` ブロックが存在し `user_invokable: true` の job のみ KLP `jobs.execute` 経由で実行可能。Agent 側で manifest を必ず再 lookup し、非公開への変更が即時反映されます
- `category` でユーザー向けタブ分け: `software_update` (Chrome 更新等)、`troubleshoot` (Office 修復等)、`catalog` (任意導入アプリ)
- `display_name` / `display_description` / `icon` は Client App UI 専用フィールド (operator UI には影響なし)

**通知 Manifest** (`notifications/*.yaml`):

```yaml
id: maintenance-2026-05-20      # 必須、notification_id として使用
priority: emergency             # info / warning / emergency
require_ack: true               # true なら Client App でモーダル + 確認ボタン強制
title: "緊急: ネットワーク機器メンテ"
body: |
  本日 22:00 から 30 分間、VPN が停止します。
  作業中の方は事前に保存をお願いします。
issued_by: "infra-team"
issued_at: "2026-05-20T12:00:00+09:00"
expires_at: "2026-05-20T23:00:00+09:00"  # 過ぎたら Client App で表示しない

target:                         # job manifest と同じ形
  groups: [tokyo-office]
  # pcs: [PC1234]
  # all: true
```

### 2.4.2 グループメンバシップ (Sprint 5 以降: server-managed)

Sprint 5 でグループ所属は **サーバ側 KV (`agent_groups` バケット)** に移動した。Agent は起動時に自分の `pc_id` で当該バケットを get + watch し、`commands.group.<name>` の購読を動的に張る/外す。

オペレータ操作:

```bash
kanade agent groups list <pc_id>                 # 現在の所属一覧
kanade agent groups add  <pc_id> <group>         # 1 つ追加 (idempotent)
kanade agent groups rm   <pc_id> <group>         # 1 つ削除 (idempotent)
kanade agent groups set  <pc_id> <g1> <g2> ...   # 全体置換 (sort + dedup)
```

または backend HTTP 経由:

```
GET    /api/agents/{pc_id}/groups          → AgentGroups JSON
PUT    /api/agents/{pc_id}/groups          (whole list replace)
POST   /api/agents/{pc_id}/groups          (one add)
DELETE /api/agents/{pc_id}/groups/{group}  (one remove)
```

KV 値の wire format:

```json
{"groups": ["canary", "wave1"]}
```

`AgentGroups::new` で sort + dedup されるので、二人のオペレータが同じ論理集合を別順序で投入しても bit-identical JSON になる (= update-only-on-change を成立させる前提)。

**バックログ**: YAML マニフェスト (`groups/*.yaml`) で動的クエリ (SQLite ベース) からメンバシップを生成して KV に流し込む reconciler は将来計画。最初の実装はオペレータが CLI / HTTP で操作する形 (CLI も backend HTTP 経由で、認証・ロール・監査を通る)。

### 2.4.3 スケジュール定義 (schedules/*.yaml)

#418 で「いつ」は単一の `when` フィールドに統合された（旧
`cron` × `mode` × `cooldown` × `auto_disable_when_done` は廃止・
互換層なし）。`when` は 2 形のどちらか:

**(1) reconcile 型（desired-state）** — poll 周期はシステム生成
（毎分・operator は書かない）。`every` が実効間隔をゲートする。

```yaml
# キッティング: 各 PC 一度きり、新規/再イメージ PC を永久に拾う
id: kitting-once
when:
  per_pc: once
job_id: kitting-setup
target: { all: true }
enabled: true
```

```yaml
# 版アップ配布: 各 PC は「現在のマニフェスト版で一度成功するまで」発火。
# job の YAML `version` を上げると、旧版でしか成功していない PC が再び
# 対象になる（= 版バンプが再配布トリガ）。`once` が版を無視して初回成功で
# 永久スキップするのに対し、`once_per_version` は版ごとに再アーム。
# dedup は backend の `execution_results.version` を見るので backend 専用
# (runs_on: agent / per_target との併用は create 時にエラー)。
id: install-kanade-client
when:
  per_pc: once_per_version
job_id: install-kanade-client
target: { groups: [dejisen] }
runs_on: backend
enabled: true
```

```yaml
# 巡回/棚卸し: 各 PC 6 時間ごと
id: inventory-hw
when:
  per_pc: { every: 6h }
job_id: inventory-hw
target: { all: true }
jitter: 5m
enabled: true
```

```yaml
# 点呼: 代表 1 台が 24h ごと（fleet 横断 dedup = backend 専用。
# runs_on: agent との併用は create 時にエラー）
when:
  per_target: { every: 24h }
```

**(2) calendar 型（時刻トリガ・Phase 2）** — 壁時計時刻で全 target に
発火（dedup なし）。`at` が時刻 (`HH:MM`) なら `days`（曜日）と組んで
繰り返し、日時 (`YYYY-MM-DD HH:MM`、ハイフン/スラッシュ/ISO `T` 可) なら
**その日時に 1 回だけ**（year 付き cron に lower、過去 year は二度と
発火しない＝one-shot）。発火時刻はスケジュールの `tz` で評価。

```yaml
# 平日 9:00 繰り返し
id: morning-greeting
when:
  calendar:
    at: "09:00"
    days: [mon-fri]            # cron DOW（名前・範囲可）。省略 = 毎日
                               # nth-weekday: tue#2 = 第2火曜（Patch Tuesday、序数 1..5）
                               # last-weekday: friL = 月末最終金曜（月次メンテ向け）
tz: local                      # 実行ホストの TZ で 9:00（minipc = JST）
job_id: show-toast
target: { all: true }
starting_deadline: 30m
enabled: true
```

```yaml
# 2026/06/10 09:00 に 1 回だけ（one-shot）
when:
  calendar:
    at: "2026-06-10 09:00"     # 日時 → one-shot（days と排他）
tz: local
```

**(3) event 型（OS イベントトリガ・加算的）** — 時計でなく **OS イベント**
で発火。`runs_on: agent` 専用（agent が自分の event source を持つ。
`backend` は validate で reject、空リストも reject）。各イベント発生ごとに
1回、freeze / active / `constraints.window` / `skip_dates` の標準ゲート込みで
発火する。現状 `startup` / `logon` / `lock` / `unlock` / `network_change`。

```yaml
# OS 起動時 / ログオン時 / ロック解除時に走らせる（agent ネイティブ）
when:
  on: [startup, logon, unlock]
runs_on: agent
job_id: boot-inventory
```

`startup` は **OS 起動ごとに1回**（host の `boot_time` でデデュープ。agent の
self-update / クラッシュ再起動など同一 boot 内の再起動では再発火しない）。
既定は遅延しても発火（OS 起動から agent 起動まで間が空いても、その boot で
未発火なら撃つ）。`starting_deadline` を付けると「OS 起動から N 以内に agent が
起動したときのみ」に限定（boot 直後の通知など prompt 用途）。`logon` / `lock` /
`unlock` は Windows サービスの session-change 通知で発火 ─ `logon`
（`WTS_SESSION_LOGON`）は **対話型セッションのユーザーログオン**（コンソール /
RDP リモート / 自動ログオンを含む。サービス / ネットワーク / バッチ等の非対話
ログオンは WTS セッションを作らないため発火しない）、`lock` / `unlock`
（`WTS_SESSION_LOCK` / `_UNLOCK`）はワークステーションのロック / ロック解除。
`network_change`（`NotifyAddrChange`）は IP アドレステーブルの変化（接続/切断/
DHCP 更新/VPN/Wi-Fi ローミング）で発火 ─ 1回の遷移が複数の生イベントを出すため
**agent 側でデバウンス**（ネットワークが落ち着いてから1回）。

**OS 対応**：イベント源があるのは `startup` のみ全 OS。`logon` / `lock` /
`unlock` / `network_change` は Windows 専用 ─ macOS / Linux agent では**決して
発火しない**。schedule は create 時に OS を知り得ない（target が OS をまたぐ）ため
create 時 reject はせず、agent が schedule をロード（reconcile）した時点で
**1回だけ** WARN ＋ synthetic skip 結果（exit 122、stderr
`skipped: when.on [unlock] never fires on macos — this OS has no unlock source`）
を出す（schedule id ＋ job version ＋ 未対応 trigger 集合ごと・agent プロセス生存中1回）。
`startup` を含む schedule は登録されたままなので `startup` は通常どおり撃つ。

**`tz`（Phase 2）** — `local`（既定・実行ホストの TZ。`runs_on: agent`
なら agent、それ以外は backend サーバー）/ `utc`。calendar の `at` も
下記 `active` の境界も**同じ `tz` で評価**する（スケジュール単位で
タイムゾーンが一貫）。

任意の `active.{from,until}`（半開区間 `[from, until)`）で有効期間を
区切れる。期間外は dormant（削除ではなく休眠）— batch campaign の
footgun-free な終了手段:

```yaml
active:
  from: 2026-07-01     # YYYY-MM-DD は tz の 0 時。RFC3339 ならオフセット優先
  until: 2026-08-01
```

> 補足: `YYYY-MM-DD` のみの境界はスケジュールの `tz` の 0 時起点で解釈
> される（Phase 1 は UTC 固定だった）。秒単位まで厳密に切りたい時は
> RFC3339（`2026-07-01T00:00:00+09:00`）で書く。

**`constraints.window`（メンテナンス窓・Phase 3）** — `active`（いつまで
有効か）とは別軸で、**毎日のどの時間帯に発火を許すか**を `"HH:MM-HH:MM"`
で指定。窓外の tick はスキップ。`tz` で評価（`active`/calendar `at` と
同じ）。`start > end` は日跨ぎ（`"22:00-05:00"` = 22 時〜翌 5 時）。主に
reconcile 巡回（「6h ごとだが発火は夜間だけ」）や日中の変更凍結向け:

```yaml
when:
  per_pc: { every: 6h }
tz: local
constraints:
  window: "22:00-05:00"   # 夜間のみ発火（agent ローカル時刻）
```

**`constraints.skip_dates`（祝日除外）** — 発火を**禁止する日**を
`YYYY-MM-DD` の配列で列挙（`tz` の壁時計日付で評価）。全 `when` 形に効く
（reconcile はその日丸ごとスキップ／calendar のその日の発火を抑止）。
組込みの祝日カレンダーは無く operator が日付を列挙する。スケジューラと
`preview` の両方が `Constraints::allows` 経由で honor する。不正日付は
create 時に reject、手編集 KV の壊れた日付は fail-closed（その日でなく
全日ブロック・`bad_skip_date` で warn）:

```yaml
constraints:
  window: "22:00-05:00"
  skip_dates: ["2026-01-01", "2026-12-25"]   # 元日・クリスマスは撃たない
```

**`constraints.require`（端末状態ゲート）** — 端末の**環境状態**が条件を
満たす時だけ発火する。現状 `ac_power`（AC 電源接続時のみ・バッテリ駆動中は
skip）、`idle`（アクティブなコンソールセッションの無入力時間が指定 humantime
以上の時のみ・「ユーザー作業中は撃たない」）、`cpu_below`（マシン全体の CPU
使用率が指定パーセント未満の時のみ・「ビジー時は撃たない」）、`network`（インター
ネット接続がある時のみ・「オンラインになるまで撃たない」）。VPN / SASE / 特定
アプリ起動中などの込み入った条件はカスタムスクリプトゲート（別スライス）で。
`wake` は別スライス。

```yaml
when:
  per_pc: { every: 1h }   # require は reconcile cadence 推奨（毎分再チェックで
runs_on: agent            #   条件成立まで待って発火。calendar 単発はその瞬間に
constraints:              #   条件未充足なら見送り）
  require:
    ac_power: true        # AC 接続時のみ
    idle: 10m             # コンソール無入力 10 分以上
    cpu_below: 20         # system CPU < 20%
    network: true         # インターネット接続あり（複数指定は AND）
```

- **`runs_on: agent` 限定**。host の電源/入力状態は agent しか読めないため
  `runs_on: backend` は create 時に reject（`when: { on }` と対称）。
- **runtime ゲート**：センシングは fire 時に毎回行う。`Constraints::allows` には
  入れない（純粋・preview が使う）ので **`preview` には反映されない**。
- **Windows-only センシング**：`GetSystemPowerStatus`（AC）/
  `WTSQuerySessionInformationW`（idle）/ `GetNetworkConnectivityHint`
  （network・**InternetAccess のみ**を「接続あり」と判定。captive portal=
  ConstrainedInternetAccess は通信が傍受されダウンロードが失敗するので不可、
  LAN のみ=LocalAccess も不可）。`cpu_below` は既存 `host_perf` の system CPU%
  （`sysinfo` の全コア平均）を**再利用**するので全 OS で評価でき、最大 host_perf
  cadence（既定 60s）ぶん古い値になりうる（「概ねビジーか」の代理としては十分・
  gate 時の単発読みより正確）。
- **非 Windows は fail-closed ＋可視化**：macOS / Linux agent では `ac_power` /
  `idle` / `network` を評価できない（判定は agent が fire 時に行う・create 時の OS
  検査はしない）。そのゲートが1つでも設定されていれば**ジョブを実行せず**、
  per-pc 完了も記録しない（ゲートが対応した時点で改めて走る）。毎分 tick でも
  WARN ＋ synthetic skip 結果（exit 122、stderr
  `skipped: constraints.require.idle cannot be evaluated on macos — not running (fail-closed)`）
  は schedule id ＋ job version ＋ 未対応ゲート集合ごとに agent プロセス生存中
  **1回だけ**、以降の tick は debug ログのみ。
- **fail-closed / 端**：AC が unknown/読めない → ac ゲートは block（確認できない
  制限ゲートは撃たない）。idle は、ヘッドレス/コンソール未接続（対話ユーザー
  不在）なら**充足扱い**（無人機は「作業中に撃たない」を自明に満たす）。
  `cpu_below` はサンプル未取得（host_perf 未ウォームアップ）なら block。比較は
  厳密 `<`（閾値ちょうどは未充足）。`network` はオフライン/LAN のみ/読めない
  なら block。
- **未充足時**：skip-this-tick。reconcile は次 tick で再チェック（実質デファー）、
  calendar 単発は `window` 同様に見送り。
- **`when: { on: ... }` との併用注意**：イベント発火（startup/logon 等）は
  そのイベントにつき 1 回で、未充足ならその機会は消費される（例: `on: startup`
  ＋ `ac_power: true` でバッテリ起動 → その起動では撃たず、後で AC を挿しても
  再試行しない）。require は AC のように**セッション中に変化しうる**初の
  ゲートなので、確実に行き渡らせたい場合は `per_pc: { every: 1h }` 等の reconcile
  cadence と併用してキャッチアップさせる。

> `constraints:` は `window` / `max_concurrent`（同時実行上限・backend のみ）/
> `skip_dates` / `require`（ac_power・idle・cpu_below・network、agent のみ）。
> カスタムスクリプト require ゲート（#631）と `wake` は別スライス。

`Schedule::validate()`（`Manifest::validate()` と対称）が create 時
（CLI / `POST /api/schedules` の両方）に検査する: `runs_on: agent`
+ `per_target` / 不正な `every` (humantime) / 不正な calendar `at`
（範囲外・日時 at と days の併用） / 不正な `constraints.window`
（書式・始端=終端）/ `constraints.require` + `runs_on: backend` / 不正な
`constraints.require.idle` (humantime) / `constraints.require.cpu_below` が
(0, 100] 範囲外 / 未登録 `job_id`（API のみ・JOBS KV 照会）/
`active.from >= until`。

**可視化（read-only エンドポイント）** — schedule の状態は 3 つの
read-only ビューで確認できる（CLI `kanade schedule {preview,status,coverage}`
+ 同名 API + SPA の Schedules ページ）:

- `GET /api/schedules/{id}/preview` — calendar schedule の次 N 回の
  発火時刻を tz 解決して列挙（`active` / `constraints.window` /
  `skip_dates` を honor）。reconcile 形は cadence を返す。
- `GET /api/schedules/{id}/status` — `enabled` / 次回発火 / 直近 1 件の
  run / 直近 24h の ok・fail・skipped 集計。`skipped` は agent がポリシー
  （deadline / revoke / version-pin / staleness / OS 非対応）でスクリプトを
  実行せずに返した結果（`ExecResult.skipped = Some(true)` →
  `execution_results.skipped = 1`）で、fail には数えない。判定はこの
  フラグのみで、予約終了コードは理由を示すだけ（実スクリプトの 126 / 127
  は失敗）。署名検証で拒否した命令（exit 123）は `Some(false)` で送られ、
  skip ではなく fail として数える。フラグを知る agent は常に true / false を
  送るので、キー欠落 = フラグ以前の旧 agent。旧 agent の結果は集計では
  従来どおり終了コードで判定し（非 0 = fail）、check_status 投影では従来の
  規則どおり exit 122–127 を投影しない。
- `GET /api/schedules/{id}/coverage` — **ロールアウト・カバレッジ**。
  schedule の `target` を**全台（オフライン含む）**に解決し、各 agent の
  最新実行結果を `ok` / `fail` / `skipped`（最新が agent の未実行結果）/
  `running`（`finished_at IS NULL`）/
  `pending`（未実行）に分類して「N 台中 M 台完了」を出す。各 agent が
  最後に実行した manifest `version` も並記（脆弱性対応の版追跡向け）。
  脆弱性対応のアプリ更新等、フリート全体への行き渡りを追跡する用途。
  `GET /api/schedules/coverage`（id 無し）は一覧用に全 schedule の
  集計カウントを 1 リクエストで返す（SPA の進捗バー）。
  集計は `execution_results.job_id` キー（`execution_results` に
  `schedule_id` 列が無いため）— 同 job を共有する複数 schedule や同 job
  への ad-hoc `exec` も同じ数字に乗る（「その job が行き渡ったか」と
  しては正しい）。母数の「全台」は `resolve_roster(.., alive_only=false)`
  で解決（dispatch は alive のみだが coverage は未実行のオフライン台も
  pending として数える）。

### 2.4.4 Agent / Backend 設定ファイル (TOML + teravars 変数展開)

Agent / Backend 自体の起動設定は **TOML + Tera テンプレート構文** で記述し、`yukimemi/teravars` crate (v0.1.5+) で読み込む。teravars は `[vars]` セクションの自己参照解決 + `system.*` context + クロスプラットフォーム判定 (`is_windows()` / `is_linux()`) + multi-file merge を一気通貫で提供します。

#### Cargo.toml 依存

```toml
[dependencies]
teravars = { version = "0.1", features = ["merge"] }   # multi-file 対応
serde    = { version = "1", features = ["derive"] }
toml     = "0.8"
```

#### Agent 設定 (`agent.toml`) の例 — **Win/Linux 同一ファイルで両対応**

```toml
[vars]
# system.host / system.os は teravars の system_context() が提供 (env を直叩きしなくていい)
hostname = '{{ system.host }}'

# OS で分岐 (is_windows() / is_linux() が組み込みヘルパー)
base = '''{% if is_windows() %}{{ env(name="ProgramData") }}\Mgmt{% else %}/var/lib/mgmt{% endif %}'''
cert_dir = '{{ vars.base }}{% if is_windows() %}\certs{% else %}/certs{% endif %}'
log_dir  = '{{ vars.base }}{% if is_windows() %}\logs{% else %}/logs{% endif %}'

version = '1.0.0'

[agent]
id          = '{{ vars.hostname }}'
nats_url    = 'nats://mgmt-server:4222'
state_db    = '{{ vars.base }}/state.db'
outbox_path = '{{ vars.base }}/outbox'

[tls]
ca_cert     = '{{ vars.cert_dir }}/ca.crt'
client_cert = '{{ vars.cert_dir }}/{{ vars.hostname }}.crt'
client_key  = '{{ vars.cert_dir }}/{{ vars.hostname }}.key'

[inventory]
hw_interval  = '24h'
sw_interval  = '24h'
net_interval = '1h'
jitter       = '10m'

[log]
path  = '{{ vars.log_dir }}/agent.log'
level = 'info'
```

`is_windows()` の分岐で 1 つの agent.toml が Linux / Windows の両 OS で動作する。配布は単一ファイルで済む。

#### Backend 設定 (`backend.toml`) の例

```toml
[vars]
hostname = '{{ system.host }}'
data_dir = '{{ env(name="MGMT_DATA_DIR", default="/var/lib/mgmt") }}'
log_dir  = '{{ env(name="MGMT_LOG_DIR",  default="/var/log/mgmt") }}'

[server]
bind       = '0.0.0.0:8080'
public_url = 'https://mgmt.example.com'

[nats]
url   = 'nats://localhost:4222'
creds = '{{ vars.data_dir }}/backend.creds'

[db]
sqlite_path = '{{ vars.data_dir }}/backend.db'

[auth]
oidc_issuer = 'https://auth.example.com/realms/mgmt'
oidc_client = 'mgmt-backend'

[log]
path  = '{{ vars.log_dir }}/backend.log'
level = 'info'
```

#### Rust 実装 (シングルファイル)

```rust
use teravars::{Context, Engine, extract_vars, resolve, system_context};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct AgentConfig {
    agent: AgentSection,
    tls: TlsSection,
    inventory: InventorySection,
    log: LogSection,
    // vars セクションは展開済みなので構造体に含めなくて OK
}

fn load_config(path: &Path) -> anyhow::Result<AgentConfig> {
    let raw = std::fs::read_to_string(path)?;
    let mut engine = Engine::new();                  // Tera + std-helpers

    let mut vars = extract_vars(&raw)?;              // [vars] を text ベースで抽出
    resolve(&mut vars, &mut engine)?;                // cross-ref を fixpoint まで解決

    let mut ctx: Context = system_context();         // system.os/arch/user/host
    ctx.insert("vars", &vars);

    let rendered = engine.render(&raw, &ctx)?;
    let cfg: AgentConfig = toml::from_str(&rendered)?;
    Ok(cfg)
}
```

#### Rust 実装 (multi-file merge を使う場合)

環境別 / ホスト別の上書きをファイル分割で実現する場合:

```rust
use teravars::{discover_config_files, load_merged, Engine, system_context};

let mut engine = Engine::new();
// /etc/mgmt/ の場合: config.toml → config.*.toml → config.local.toml の順に発見
let files = discover_config_files("/etc/mgmt")?;
let merged = load_merged(files.iter(), &mut engine, &system_context())?;
let cfg: AgentConfig = merged.config.try_into()?;
```

ファイル配置イメージ:
```
C:\ProgramData\Mgmt\
├── config.toml              # 共通設定 (base)
├── config.prod.toml         # 本番固有上書き
├── config.{{ host }}.toml   # ホスト固有上書き (include で動的取り込み)
└── config.local.toml        # ローカルデバッグ用 (最後に勝つ)
```

config.toml 側で `include` ディレクティブを使うとホスト固有設定を動的に取り込める:
```toml
include = [
  "config.prod.toml",
  "{{ system.host }}.toml",   # PC1234.toml が存在すれば取り込み
]

[vars]
hostname = '{{ system.host }}'
# ...
```

#### teravars が提供する組み込み関数・フィルタ (本構成で活躍するもの)

| 関数 / フィルタ | 用途 |
|---|---|
| `{{ system.host }}` | ホスト名 (cross-platform。`COMPUTERNAME` / `HOSTNAME` を意識不要) |
| `{{ system.os }}` | `"windows"` / `"linux"` / `"macos"` |
| `{{ system.arch }}` | `"x86_64"` / `"aarch64"` |
| `{{ system.user }}` | 実行ユーザ名 |
| `{% if is_windows() %}` | OS 分岐 (cross-OS 設定の核) |
| `{{ env(name="X", default="Y") }}` | 環境変数 + デフォルト |
| `{{ vars.x \| hash }}` | 文字列ハッシュ (Agent ID 生成等に) |
| `{{ vars.x \| port_offset(start=4222, range=10) }}` | ポート割当 (NATS クラスタ各ノードのポート計算等) |

特に `is_windows()` / `system.host` / `env(default=...)` の 3 つで、Win/Linux 両対応の単一 agent.toml が現実的に書ける。これが teravars 採用の最大の利点です。

## 2.5 配信戦略

### 2.5.1 Jitter (時刻ばらけ)

各 Agent は受信したコマンドの `jitter` 値に従い、0〜jitter のランダム時刻まで sleep してから実行する。3000 台同時発火を防ぐ。

```rust
let jitter = cmd.jitter.unwrap_or(Duration::from_secs(60));
let delay = rand::thread_rng().gen_range(0..jitter.as_secs());
tokio::time::sleep(Duration::from_secs(delay)).await;
execute(cmd).await;
```

### 2.5.2 Wave (段階配信)

YAML の `rollout.waves` に従い、Backend のスケジューラが時間差で各グループに publish する。

```
T+0m   ──> commands.group.canary  (50台)   ← 様子見
T+30m  ──> commands.group.wave1   (500台)
T+60m  ──> commands.group.wave2   (1000台)
T+90m  ──> commands.group.wave3   (1450台)
```

途中の wave で失敗率が閾値を超えたら、後続を自動停止する (自動 abort)。

### 2.5.3 スケジュール

Backend 内の `tokio-cron-scheduler` が、`schedules` KV の定義（`when` を内部で poll cron + dedup ポリシーに lower、§2.4.3）に基づき publish。

Agent 内の定期タスク (1 時間ごと HW チェック等) は Agent 内 `tokio::time::interval` で完結 (中央スケジューラ不要)。

### 2.5.4 KV 駆動の動的設定変更

Agent は `agent_config` KV を watch し、設定変更を即時反映する。
例: 「全 Agent のインベントリ採取頻度を 1h → 30m に変更」を KV 1 書き換えで全台適用。

```rust
let kv = jetstream.get_key_value("agent_config").await?;
let mut watcher = kv.watch("schedule.>").await?;
while let Some(entry) = watcher.next().await {
    update_local_schedule(entry.value);
}
```

## 2.6 バージョン管理と緊急停止 (3層防御 + オフライン補強)

「古い版が実行される」「revoke 済が実行される」「実行中を止めたい」の 3 つを、それぞれ独立した層で防ぐ。Sprint 6.x までで全層実装済。v0.23.0 で agent 側 local_scheduler が入って **オフライン端末からも実行が起こる** ようになったため、Layer 2 に *staleness policy* を追加してオフライン時の挙動を Manifest 側から制御できるようにしました (詳細は §2.6.2)。

### 2.6.1 第1層: Broker 滞留メッセージの置換

`STREAM_EXEC` を `max_messages_per_subject = 1` + `DiscardPolicy::Old` で構成。同一 subject (`commands.pc.{pc_id}` / `commands.group.{name}` / `commands.all`) への publish は常に最新の 1 通のみ broker 上に残り、旧版は自動的に破棄される。

オフラインだった Agent が復帰すると、durable consumer (`DeliverPolicy::LastPerSubject`) で **subject ごとの最新 1 通だけ** を replay 受信する。途中の中間版は配送されないので、古い命令が遅延配送される事故を構造的に防ぐ (v0.22.1)。

```rust
// crates/kanade-shared/src/bootstrap.rs
js.create_or_update_stream(StreamConfig {
    name: STREAM_EXEC.into(),                  // "EXEC"
    subjects: vec!["commands.>".into()],       // commands.all / commands.group.X / commands.pc.Y
    max_messages_per_subject: 1,
    discard: DiscardPolicy::Old,
    max_age: Duration::from_secs(7 * 24 * 60 * 60),
    ..Default::default()
})
.await?;
```

> spec 初版で言及していた `DEPLOY` stream / `commands.deploy.>` subject は v0.22.1 で `STREAM_EXEC` / `commands.>` に統合済。配信経路を 1 本化することで、ad-hoc exec / 定期 schedule / 緊急コマンド すべてが同じ replay 経路に乗る。

### 2.6.2 第2層: 実行直前の version 照合 + staleness policy

Agent は `handle_command` の冒頭で常に 2 つの KV を引いて判定する:

- `BUCKET_SCRIPT_CURRENT` (`script_current`) — `cmd_id → version` を保持。backend が `kanade exec` 時に `kv.put(manifest.id, manifest.version)` で更新する。受信した `Command.version` と KV 値が一致しなければ skip。
- `BUCKET_SCRIPT_STATUS` (`script_status`) — `cmd_id → "ACTIVE" | "REVOKED"`。`kanade revoke <cmd_id>` (backend API 経由。audit は backend が呼び出しアカウント付きで記録) / `POST /api/scripts/{cmd_id}/revoke` で REVOKED に更新。REVOKED なら skip。

```rust
// crates/kanade-agent/src/commands.rs::handle_command (抜粋)
if let Some(cur) = &script_current
    && let Ok(Some(entry)) = cur.get(&cmd.id).await
{
    if String::from_utf8_lossy(&entry) != cmd.version { return Ok(()); }
}
if let Some(sta) = &script_status
    && let Ok(Some(entry)) = sta.get(&cmd.id).await
{
    if String::from_utf8_lossy(&entry) == SCRIPT_STATUS_REVOKED { return Ok(()); }
}
```

#### オフライン時の課題

`runs_on: agent` schedule (v0.23.0) は agent の **キャッシュされた `BUCKET_JOBS` 値** から直接 fire する。Agent が broker から切れた状態でも fire できる代わりに、`script_current` / `script_status` のリアルタイム照合が出来ない。素の `if let Ok(Some(_))` 判定だと get 失敗が "skipped check" として**暗黙的にスキップ（素通り）**されてしまい、revoke 済の命令でも走ってしまう。

これは Manifest によって許容度が違う:
- **緊急パッチ・コンプライアンス系**: 必ず最新が確認できない端末では走らせたくない (= 安全側に倒して skip)
- **インベントリ・kitting**: オフラインでも採取は続けたい (= cache で実行 OK)

Manifest 側に *staleness policy* を持たせて、Agent が fire 時にこの判断を切り替える。

#### Staleness policy のスキーマ (Manifest.staleness)

```yaml
# jobs/urgent-patch.yaml — 必ず最新版確認できないと走らせない
id: urgent-patch
version: "2.5.1"
execute:
  shell: powershell
  script: Install-Hotfix KB1234567
  timeout: 600s
staleness:
  mode: strict
  max_cache_age: 0s     # broker と現に繋がってないと skip

# jobs/inventory-hw.yaml — offline でも走らせる
id: inventory-hw
version: "1.0.0"
execute:
  shell: powershell
  script: Get-WmiObject Win32_ComputerSystem
  timeout: 600s
staleness:
  mode: cached          # cache 値で照合、age 制約なし

# jobs/legacy.yaml — version pin / revoke 自体を無視 (旧 manifest 互換)
id: legacy
version: "0.1.0"
execute:
  shell: cmd
  script: echo hello
  timeout: 600s
staleness:
  mode: unchecked
```

#### Mode 仕様

| mode | 動作 | 用途 |
|---|---|---|
| `strict` | KV `script_current` / `script_status` の cached age が `max_cache_age` 以内 → cache で照合。超過なら broker に live `kv.get()` を試みる。fail なら skip (exit 127, "staleness check failed") | 緊急パッチ、コンプライアンス、セキュリティ系 |
| `cached` (default) | cache 値で照合。`max_cache_age` は無視。エントリ自体が無ければ ACTIVE & version match 扱い (silently proceed) | インベントリ、kitting、hourly check 等の "ベストエフォート系" |
| `unchecked` | version pin / revoke ともに無視。受け取った Command をそのまま実行 | ローカル完結 / idempotent / 旧 Manifest 互換 |

#### `max_cache_age` の意味と staleness 計測

`strict` mode のみで意味を持つ。「最後に **broker と同期できていた瞬間** から、どれだけ経過しても cache を信用していいか」 のタイムアウト。

KV watch は push 型なので、agent が broker に接続している限り cache は常に「同期済」と見なせる (broker 側で更新があれば push されてくる契約)。切断された瞬間にタイマーのカウントが開始され、再接続で 0 にリセットされます。

```rust
// 概念実装
let staleness = match (client.state(), last_connected_at) {
    (State::Connected, _) => Duration::ZERO,
    (_, Some(t)) => Instant::now() - t,
    (_, None)    => Duration::MAX,            // 起動から一度も繋がってない
};
if matches!(policy.mode, Mode::Strict) && staleness > policy.max_cache_age {
    return publish_skipped_result(cmd, ExitCode::StalenessExceeded /* 127 */);
}
```

| 設定例 | セマンティクス |
|---|---|
| `mode: strict, max_cache_age: 0s` | fire 時点で **online でなければ skip** |
| `mode: strict, max_cache_age: 5m` | 直近 5 分以内に broker に繋がっていれば OK (一時的な瞬断は許容) |
| `mode: strict, max_cache_age: 1h` | 1 時間以内の disconnect なら OK。それ以上の長期 offline は skip |
| `mode: cached` | 期限なし。cache さえあれば走る |

デフォルトは `strict` ではなく **`cached`** にする。歴史的に v0.22 以前は無条件で素通りだったので、後方互換のためデフォルトを変えると既存 Manifest が突然 skip するリスクがある。緊急系は **明示的に `strict` を書くポリシー** にする。

### 2.6.3 第3層: 実行中の緊急停止

Agent は run ごとに 1 つの kill switch (`kill.rs`) を最外殻 (jitter より前) で arm する。switch は `kill.{exec_id}` の NATS subscribe (backend API / CLI の remote kill 用) と、プロセス内 registry (Client App の `jobs.kill` 用。broker 不要) の両方を 1 つの latch に合流させ、jitter・local slot 待ち・子プロセス実行・retry backoff のどの状態でも同じ switch で kill を受ける。子プロセス実行中は `tokio::select!` で `child.wait()` / switch / timeout を競争させる。registry の entry は run 終了 (switch の Drop) で除去される。kill / timeout ではホストと子孫をまとめて終了し (Windows: Job Object、unix: ホストを `setsid` で専用プロセスグループに置き、グループへ SIGTERM → 最大 5 秒後に SIGKILL)、結果は `ExecOutcome::Killed` (timeout なら `ExecOutcome::Timeout`) として publish される。正常終了時はツリーに触れないので、スクリプトが意図的に切り離したデーモンは残る (`run_as: user / system_gui` の Win32 path も oneshot bridge 経由で同じ経路に集約。macOS はこの tokio::process 経路そのものを `launchctl asuser` で包む)。

```rust
// crates/kanade-agent/src/process.rs::run_command_with_kill (抜粋)
let mut kill_sub = client.subscribe(subject::kill(&job_id)).await?;
client.flush().await.ok();                  // SUB が登録される前の publish 取りこぼし防止

tokio::select! {
    status = child.wait() => { /* 正常終了 */ }
    msg    = kill_sub.next() => {
        child.kill().await.ok();
        OutcomeInner::Killed
    }
    _ = tokio::time::sleep(timeout) => {
        child.kill().await.ok();
        OutcomeInner::Timeout
    }
}
```

> **オフライン端末への kill は原理的に届かない。** EVENTS stream に乗せても、子プロセスはもう走ってしまっているし、再接続のタイミングと kill の timing が合わない。kill は「現在 online でかつ走っている」 ケース専用と割り切る。「絶対走らせたくなかった」 ケースは Layer 2 の revoke + staleness で防ぐ。

### 2.6.4 オペレータの「止める」 操作 3 パターン

SPA / CLI から見える "止める" 操作は以下の 3 種類。それぞれ Layer 1 / 2 / 3 のどれを発火させればいいかを整理する:

| 起点 | in-flight 子プロセス | publish 済 / 未実行 | 未来の fire |
|---|---|---|---|
| **(a) exec を発行したやつを止める** (`kanade kill <exec_id>` / SPA) | `kill.{exec_id}` publish (Layer 3) | `kanade revoke <cmd_id>` (Layer 2) | n/a (単発 exec) |
| **(b) job (Manifest) を delete** (`kanade job delete <id>` / SPA) | (a) と同じ kill cascade | **delete 操作が同時に `script_status: REVOKED` を書く** (cascade 必須) | `BUCKET_JOBS` から消えるので backend `kanade exec` 経路 + agent local_scheduler 経路ともに自然停止 |
| **(c) schedule を無効化** (`enabled: false` / SPA) | オプション (`--cascade-kill`) | オプション (`--cascade-revoke`) | `BUCKET_SCHEDULES` の `enabled: false` で backend scheduler + agent local_scheduler ともに次 tick で停止 |

#### (b) job delete の cascade 必須化

job を消すと「以後の `kanade exec` は失敗する」 (manifest 不在) し、agent の local_scheduler も `BUCKET_JOBS` の delete event を watch で受けて該当 schedule を de-register する。ところが **既に publish 済で agent 受信前 / agent 実行直前** の Command については、agent が `script_current` / `script_status` を見るだけだと止められない (manifest 削除自体は KV 上に痕跡を残さない)。

そこで job delete 操作の中で、必ず以下を atomic に行う:
1. `BUCKET_JOBS` から該当 manifest を削除
2. `BUCKET_SCRIPT_STATUS` に `cmd_id → REVOKED` を書く
3. AUDIT に "job delete (with revoke cascade)" イベントを emit

オペレータが「やっぱり元に戻したい」 場合は、`kanade job create` で復活 (BUCKET_JOBS 戻し) → `kanade unrevoke <cmd_id>` (script_status ACTIVE 戻し) の手順。

#### (c) schedule 無効化の 2 段階

「これ以降の cron 発火を止めたい」 だけ (ふつう) と、「**今走ってる / 既に投げた fire も全部止めたい**」 (緊急) は意図が違う。spec として両モードを用意する:

- **soft disable** (`enabled: false` のみ): 次 tick 以降の発火を止める。in-flight は触らない。
- **hard disable** (`enabled: false` + `--cascade-revoke` + `--cascade-kill`): in-flight kill (Layer 3) + 未実行を REVOKED (Layer 2) + cron 停止 (Layer 1 相当の "未来分") をワンショットで実行。

SPA の Schedule ページに「無効化」 (default = soft) と「無効化 + 進行中も停止」 (hard) の 2 ボタンを置く。CLI は `kanade schedule disable <name>` / `kanade schedule disable <name> --cascade`。

> **実装状況**: `--cascade` = **Layer 2 revoke** (`script_status.{job_id} = REVOKED`)、`--cascade-kill` = **Layer 3 kill** (ともに `crates/kanade-backend/src/api/schedules.rs` の `disable` ハンドラ)。2 つは **直交フラグ**で、kill は *実行中* を、revoke は *queued/未来* を止める — full hard-disable は両方渡す。`--cascade-kill` は `execution_results` の in-flight 行 (`job_id = ? AND finished_at IS NULL`) から exec_id を列挙し、各 `kill.{exec_id}` を publish する (kill が `kill.{job_id}` → `kill.{exec_id}` に進化したため §2.6.3 — 列挙が必要)。kill を**別フラグ・明示 opt-in** にした理由: (1) 実行中スクリプトの強制終了は部分適用リスク (インストール途中等) があり `revoke` (未来を止めるだけ) より破壊的、(2) online 限定なので revoke と混ぜると「全部止まった」錯覚を生む。best-effort (DB エラーは「kill 0 件」に degrade、disable 自体は成功)。

#### (d) fleet 全体を凍結する (`kanade freeze` — #418 Phase 5)

(a)〜(c) は **job / schedule / exec 単位**の停止。これに対し **fleet 全体の「変更凍結」**を一発でかける緊急スイッチが `kanade freeze`。`fleet_config`/`freeze` KV シングルトンに [`Freeze`](`kanade-shared/src/manifest.rs`) を書き、backend scheduler と全 agent の local_scheduler が **tick 冒頭でゲート**して全スケジュールの発火を止める。

- **粒度**: fleet 全体 (job/schedule を問わない)。空窓 = 無期限凍結 (インシデントの非常停止)、`{from, until}` = 計画凍結 (年末変更凍結など、自動 thaw)。
- **どの層か**: **3 層の手前・上流**。freeze は「未来の fire を tick で止める」だけで、**Layer 1/2/3 のどれにも触らない** — publish 済みの Command も実行中の子プロセスも止めない。「絶対走らせたくない」「今すぐ全部殺す」は引き続き revoke(L2) / kill(L3) の領分。
- **`handle_command` には入れない** (意図的): 入れると operator の手動 `kanade exec/run` まで凍結され、インシデント中の復旧オペが止まる。freeze は **スケジュール自動化を止め、手動オペは通す**。
- **オフライン**: agent は `fleet_config` を **watch するバックグラウンドタスク**で freeze を `State` にミラーし、`local_tick` はそのキャッシュを読む (per-tick KV get なし)。online 中にかかった freeze は last-known として**オフラインでも効く**が、agent が**既にオフラインの間**に新規にかけた freeze は再接続して watch が再 seed するまで届かない (KV を読めないため)。起動時はリコンサイル前に同期 prime して、boot 直後の tick が起動前 freeze を素通りしないようにする。
- **fail-safe**: 壊れた freeze blob は frozen 扱い (constraints.window の fail-closed と同方向) — 雑な手編集で凍結を素通りさせない。

#### 「止める」操作の粒度マップ (まとめ)

| 操作 | 粒度 | コマンド | 止める対象 | オフライン |
|---|---|---|---|---|
| **freeze** | 🌐 fleet 全体 | `kanade freeze set` | 全 schedule の**未来の発火** (tick ゲート) | キャッシュ済みなら効く / 凍結中の新規設定は届かない |
| schedule 無効化 (soft) | 📋 schedule | `kanade schedule disable <id>` | その schedule の未来 tick (`enabled:false`) | — (次 tick で停止) |
| schedule 無効化 (hard) | 📋 schedule + L2 | `kanade schedule disable <id> --cascade` | 上 ＋ **L2 revoke** (job 単位) | ✅ 再接続時 skip |
| **L2 revoke** | 📦 job | `kanade revoke <job_id>` | その job 由来の**未実行 Command を skip** | ✅ 再接続時も skip |
| **L3 kill** | ⚡ exec (1 発火) | `kanade kill <exec_id>` | **実行中の子プロセス**を kill | ❌ online のみ |

粒度は **fleet 全体 → schedule → job → exec** と細かくなり、時間軸では **未来を止める** (freeze / disable / `enabled:false`) → **未実行を止める** (L2 revoke) → **実行中を止める** (L3 kill) と下りていく。freeze は最も粗い「fleet 全体の未来」を埋めるピース。

### 2.6.5 イベント永続化 (revoke の遅延配送)

`BUCKET_SCRIPT_STATUS` の KV watch は agent が online な瞬間しか push を受け取れない。長期オフライン端末が再接続したとき、最新状態 (= REVOKED) は KV watch の初期スナップショットで取得できるが、**「いつ revoke されたか」「途中 unrevoke を経由したか」 は KV だけだと再構成できない**。これは Layer 2 が「最新状態だけ知っていれば十分」という設計であるため、原則として問題ありません。

ただし、運用上の AUDIT のため、revoke / unrevoke / job delete / schedule disable は同時に EVENTS stream にも publish する (`events.scripts.revoked.{cmd_id}` 等)。AUDIT projector が SQLite に監査ログとして記録し、SPA の Audit ページに表示します。

### 2.6.6 オフライン端末からの fire を考慮した実装責務分担

| 責務 | 配置 |
|---|---|
| Layer 1 stream config (`max_messages_per_subject: 1`) | backend bootstrap (`kanade-shared/src/bootstrap.rs`) |
| Layer 2 KV watch + cache + staleness check | agent (`commands::handle_command` + 新規 `staleness::Tracker`) |
| Layer 2 cascade on job delete / schedule hard-disable | backend HTTP API + CLI |
| Layer 3 `kill.{exec_id}` subscribe + child kill | agent (`process::run_command_with_kill`) |
| 最終 connectivity timestamp 追跡 | agent (`async_nats::Client::state()` watcher) |
| Exit code 規約 (予約コード `121` = 実行中に agent が再起動し結果不明、`122`, `124–127` = agent が実行を見送った理由、`123` = 署名検証で拒否。skip の判定は `ExecResult.skipped` フラグのみで終了コードでは行わず、121 と 123 は failure として数える) | shared (`kanade-shared/src/wire/result.rs`) |
| SPA UI (revoke / kill / cascade ボタン + 進行中 job 一覧) | `kanade-backend/web/src/pages/` (Jobs / Schedules / Results) |

### 2.6.6a 命令受理台帳 (durable admission ledger)

NATS で届いた命令 (`CommandSource::Nats`。live 購読と JetStream replay の両方) は、agent 再起動をまたいで「受理済み」を覚えるローカルの台帳 (`<data_dir>/admission/`) を経由する。メモリ上の request_id キャッシュ (FIFO 1,024 件) は廃止した。agent 自身の local scheduler が合成する命令は対象外で、従来どおりの扱い。

**受信順序** (live / replay 共通。`command_intake`):

1. 宛先確認 (自ホスト宛か)。
2. 来歴検証 (既存の verifier と enforcement の挙動は不変)。
3. 台帳へ受理記録。
4. JetStream メッセージを ack。

検証は記録の前に行うので、不正・未署名のメッセージが正規の request_id を予約することはない。ack は受理記録の後、または終端の拒否判断 (宛先外 / verifier による拒否 / 受理済み id の重複 / 解釈不能) の後だけ。台帳に書けない場合 (disk full、権限) は受理でも起動の許可でもなく、起動せず ack もしない (ログと obs event `command_admission_unavailable` で可視化。JetStream は未 ack として再配信する)。

**台帳**: キーは `(pc_id, request_id)`、1 request 1 ファイル (結果 outbox と同じ tmp → fsync → rename、unix ではディレクトリも fsync)。レコードは受信 payload の SHA-256 fingerprint、受信バイト列と署名ヘッダ、検証済み command、`result_id` (受理時に固定)、状態を持つ。同じ id が同時に live と replay から届いても受理は 1 回。同じ id で fingerprint が異なるものは conflict で、実行せず、obs event `command_admission_conflict` (security 診断) を出す。

**状態**: `pending` (受理済み・未起動) → `launching` (副作用の直前に永続化。launching / running を兼ねる) → `finished` (結果 + outbox 投入済みフラグ)。結果は先に台帳へ書いてから outbox へ入れ、その後に投入済みにするので、「台帳にも outbox にも無い結果」は生じない。重複配信は起動せず、`finished` で未投入の結果は再投入するだけ。結果アップロードの失敗が再実行を起こすことはない。

**クラッシュ境界** (agent 起動時、NATS 接続を待たずに実行):

| 状態 | 起動時の扱い |
|---|---|
| `pending` | 保存した payload を現在の鍵で再検証し、deadline / staleness / revoke / version pin を起動時点で再評価してローカルで実行。鍵の失効などで検証に通らなければ拒否結果 (exit 123) で終端する |
| `launching` | 結果不明。プロセスがまだ動いているか、終わっているかを区別できないので**自動で再起動しない**。`exit 121` (`EXIT_RESTARTED_OUTCOME_UNKNOWN`、`skipped=false` なので failure) を 1 回だけ報告する。メッセージは「agent restarted during execution; outcome unknown」。`result_id` は受理時に固定した値 (started イベントと同じ) を使うので、開始イベントが作った行を閉じ、再起動を繰り返しても 1 行に収束する |
| `finished` + 未投入 | outbox へ再投入 |

意図的な再実行は新しい request_id で行う。署名付きの in-process retry policy (1 回の受理 = 1 回の実行の内側) は従来どおり。

**保持と容量**: 終端レコード (tombstone) は、結果が outbox (および隔離領域) から無くなっていることを条件に、`COMMAND_STREAM_MAX_AGE` (broker の保持 7 日) に `ADMISSION_CLOCK_ALLOWANCE` (1 日) を足した期間、受理または完了の遅い方から数えて保持する。legacy 命令は自前の期限を持たないため、broker が再配信しうる間は id を覚えておく必要がある。未解決のレコード、未投入の結果、読めないレコードは GC しない。時計が巻き戻った場合も削除しない。上限は件数 (100,000) とバイト数 (1 GiB) で、到達したら GC を 1 回試し、それでも空かなければ新規の受理を可視的に拒否する (生きているレコードは退避しない)。

**保証と限界**: これは durable な重複抑止であって、副作用の exactly-once 保証ではない。起動前に記録するので、記録後・起動前にクラッシュすると何も起動していないのに `launching` として残る窓があり (その場合も 121 で報告し、再起動はしない)、起動が不確かな命令は保守的に再起動しない。disk のロールバック、台帳の削除、agent の再インストールは保証の範囲外 (台帳が失われると、broker に残っている命令は再び受理されうる)。

### 2.6.7 まとめ表 (operator 視点)

| 防ぎたい事 | どこで | 経路 | オフライン端末 |
|---|---|---|---|
| 古い版が agent に届く | broker | `STREAM_EXEC` + LastPerSubject replay | ✅ 再接続時に最新だけ受信 |
| 受信したけど古い / revoked を実行する | agent | KV `script_current` / `script_status` 照合 + staleness policy | `strict` で skip / `cached` で実行 (Manifest 側で選択) |
| 既に走っているプロセスを止める | agent | `kill.{exec_id}` subscribe + `child.kill()` | ❌ 不可 (online のみ) |
| job 削除 = 派生する exec / schedule fire も止める | backend + agent | delete 操作が `script_status: REVOKED` cascade | Layer 2 経由で次回 fire 時に skip |
| schedule 無効化 = soft / hard 選択 | backend + SPA | enabled: false (soft) / + revoke + kill cascade (hard) | Layer 2 経由で次回 fire 時に skip |

**重要**: kill signal 経路は MVP 段階から必ず仕込むこと。後付けは既存スクリプト全てに kill 経路を埋め込む作業になり困難。staleness policy も同様で、Manifest schema に **mode フィールドが入った瞬間に旧 manifest は `cached` (= 互換動作) として解釈される** 設計にしておけば後付け破綻を防げる。

## 2.7 セキュリティ

### 2.7.1 NATS 接続認証

- **方式**: mTLS (相互 TLS)
- **クライアント証明書**: 各 Agent に個別配布、CN = `pc_id`
- **権限**: Subject 単位の publish/subscribe 制限を NATS 認可ファイルで定義

Agent は `commands.pc.{自分のID}` `commands.all` `commands.group.*` に subscribe、`inventory.{自分のID}.*` `results.*` `heartbeat.{自分のID}` に publish のみ許可。

### 2.7.2 Backend API 認証

- **方式**: OIDC (Keycloak / Auth0 / Azure AD 等)
- **トークン**: JWT (短命) + refresh token
- **認可**: RBAC (admin / operator / viewer 等のロール)
- **承認フロー**: 本番配信は 2 名承認制 (DB に承認状態を保持)

### 2.7.3 スクリプト署名

- 配信スクリプトは Authenticode 署名を必須化
- Agent 側で署名検証してから実行
- Object Store からファイル取得時は SHA256 ハッシュ検証

### 2.7.4 監査

- 全 API 呼び出しを `AUDIT` Stream に publish
- `audit_log` テーブルに投影し、検索可能化
- 操作者・対象・操作内容・時刻・IP を記録

### 2.7.5 コマンド envelope (`kanade.command.v2`)

従来のコマンド (legacy) は、シリアライズした `Command` のバイト列そのものに Ed25519 署名を付けたもので、署名は任意の宛先に対して、かつ (backend 鍵なら) 期限なしに有効である。そのため捕捉したコマンドを別ホストへ、あるいは後日に再送できる。v2 envelope は宛先と絶対期限を **署名対象のバイト列の内側** に入れてこの 2 点を塞ぐ。

> 本節の時点では backend はまだ envelope を発行しない。Agent が先に検証できるようにして、切り替え前に Agent を更新できるようにするための段階である。legacy コマンドの扱いは変わらない。

**形式** (`kanade-shared` の `CommandEnvelope`。ローカルで組み立てる `Command` とは別の型):

```json
{
  "kind": "kanade.command.v2",
  "target_pc_id": "PC001",
  "expires_at": "2026-10-04T00:00:00Z",
  "command": { "...既存の Command (request_id / exec_id / 実行オプション / retry / finalize / script か script_object の digest を含む)..." }
}
```

- 署名は既存の仕組み (Ed25519、鍵リング、`Kanade-Sig*` ヘッダ、署名時刻 + body) をそのまま使い、**受信した body の生バイト列** に対して検証する。再シリアライズした値には検証しない。`sign_envelope` が一度だけシリアライズしたバイト列とヘッダを返す。
- 受信側は `kind` で判別する。`kind` を持たないものは legacy `Command` で、従来どおりに扱う。`kind` が未知・null・文字列以外のものは、legacy にフォールバックせず拒否する。
- 署名検証 → `kind` → `target_pc_id` → `expires_at` → 時刻検査の順で判定する。署名が通る前に時計や宛先の異常を報告しないので、偽造バイト列で報告を誘発できない。

**宛先**: `target_pc_id` はその Agent の登録済み pc_id と **完全一致** (大文字小文字を区別) しなければならない。他ホスト宛の envelope は、自ホストの subject に届いても (攻撃者が作った push consumer 経由も含めて) 拒否する。欠落・空も拒否する。

**期限**: `expires_at` は必須の絶対 UTC 時刻で、コマンドの **開始** 期限である。実行中のプロセスを止める指示ではない。実効的な開始期限は `expires_at` と `Command.deadline_at` の早い方。受信時と、起動直前 (jitter・スロット待ち・スクリプト取得の後、および認可された再試行の開始前) の 2 回以上確認する。再試行が期限切れで始められないときも、先行試行の実結果は破棄せず、その旨を stderr に追記して報告する。

**検証失敗の扱い**: envelope が検証に失敗した場合は、enforcement の ON/OFF に関係なく拒否する (新しい経路なので互換性を保つ対象がない)。拒否時は警告ログと署名 outcome イベントだけを出し、`ExecResult` は発行せず、`request_id` の重複排除も消費しない。宛先でないホストについて実行結果を作らないためである。enforcement フラグの意味は legacy に対して従来どおり。

**時計ポリシー** (ホストが締め出されないよう、保守的に定めている):

| 規則 | 値 |
|---|---|
| 署名時刻の未来側許容 (`FUTURE_SKEW_ALLOWANCE`) | 1 時間 (break-glass 窓が既に必要としているものと同じ) |
| 有効期間の上限 (`MAX_ENVELOPE_VALIDITY`、署名時刻から `expires_at` まで) | 7 日 (Agent が独立に強制する) |
| 通常コマンドの過去側 | 上限なし。`expires_at` のみで制限される |
| break-glass 鍵 | 鍵固有の `max_age` を上に重ねて適用する |

- 許容を超えた未来の署名時刻、`expires_at` が署名時刻より前 (逆転)、上限超過の有効期間は拒否する。
- 時計に関する拒否は、署名失敗とは別の outcome として既存の署名 outcome イベントに出す: `command_signature_clock_ahead` (署名時刻が未来すぎる)、`command_signature_expired` (開始期限または鍵の `max_age` 超過)、`command_signature_misaddressed` (宛先違い)、`command_signature_envelope_rejected` (宛先/期限の欠落・不正、逆転、上限超過など)。改ざんは従来どおり `command_signature_invalid`。
- **前提**: ホストの時刻は信頼できるものとする。任意の時計巻き戻しは壁時計ベースの鮮度上限を破る (巻き戻したホストは、実際には失効した envelope を受理する)。本方式は再送可能な窓を狭めるものであって、時計を攻撃者が制御できるホストに対する保証ではない。
- **運用上の注意**: Agent の時計が backend より 1 時間を超えて **遅れて** いると、正規の envelope が全て「未来の署名」として拒否される。envelope への切り替え前に、ホストの時刻ずれを観測しておくこと。
- JetStream の保持は 7 日で、有効期間の上限と同じである。7 日を超えてオフラインだった Agent は、正規の replay でも失効した envelope を拒否する。これは意図した挙動である。

**対応プロトコルの報告**: Agent は heartbeat の `command_protocols` に、自分が検証できるプロトコル (`["legacy", "kanade.command.v2"]`) を載せる。`command_keys` / `enforcing` と同じ三状態で扱う: 未報告 (NULL、ping 返信や旧 Agent) は「不明」であり「legacy のみ」とは推定しない / 空配列 (報告済みで何も検証できない) / 対応済みの集合。Backend は `agents.command_protocols` (JSON 配列、NULL 可、DEFAULT なし) に投影し (ping で省略されても保存済みの値は消さない)、`GET /api/agents` で `command_keys` / `enforcing` の隣に返す。これが後続のタスクで、ホストを envelope 受信に切り替える条件になる。

## 2.8 信頼性・可用性

### 2.8.1 メッセージ配信保証

| レイヤー | 仕組み |
|---|---|
| クライアント自動再接続 | `async-nats` 標準機能 (デフォルト無限リトライ) |
| Broker 障害時の透過切替 | NATS クラスタ (3 ノード)、複数 URL 指定で自動 failover |
| メッセージ永続化 | JetStream (ディスク永続化、replica=3) |
| オフライン Agent への配信 | JetStream durable consumer (復帰時に未受信分配信) |
| Agent プロセス再起動跨ぎ | durable consumer + ローカル outbox (必要時のみ) |

### 2.8.2 Agent オフライン対応

- Agent オフライン時の `inventory` / `events` / `results` は Agent 側でローカル保存し、再接続時に publish (outbox パターン、必要時のみ)
- Broker 側はオフライン Agent 宛コマンドを `STREAM` で保持し、復帰時に配信

### 2.8.3 緊急コマンドの TTL

緊急性のあるコマンドは Stream の `max_age` を 1 時間に設定し、長期オフライン端末に古い命令が届かないようにする。

## 2.9 監視・運用

### 2.9.1 メトリクス

- **エクスポート**: Prometheus 形式 (Backend, NATS 両方)
- **収集対象**:
  - Agent オンライン数 / オフライン数
  - メッセージ rate (subject 別)
  - JetStream ストレージ使用量
  - Backend API レイテンシ・エラー率
  - 配信ジョブの成功率・失敗率
- **可視化**: Grafana

### 2.9.2 ロギング

- **Backend**: `tracing` + 構造化 JSON ログ
- **Agent**: `tracing` + Windows イベントログ + ローカルファイル
- **集約**: Loki / Elasticsearch (任意)

### 2.9.3 アラート

- 配信失敗率が閾値超過
- Agent オフライン率が閾値超過
- NATS / Backend プロセスダウン
- JetStream ストレージ逼迫

### 2.9.4 バックアップ

- **JetStream**: `nats stream backup` で日次バックアップ
- **SQLite**: ファイル単位コピー (`sqlite3 .backup` で hot backup)
- **設定 Git リポジトリ**: 通常の Git 運用

## 2.10 デプロイ構成とサイジング

### 2.10.1 1 サーバー構成 (Phase 1〜2)

**論理構成 (Linux / Windows Server 共通):**

```
┌─────────────────────────────────────────────┐
│  サーバー 1 台 (Linux または Windows Server)  │
│                                              │
│  ┌──────────────┐    ┌────────────────────┐ │
│  │ backend      │    │ nats-server        │ │
│  │ + axum API   │    │ + JetStream        │ │
│  │ + SPA 配信   │    │   ├─ Stream        │ │
│  │ + Scheduler  │    │   ├─ KV            │ │
│  │ + Projector  │    │   └─ Object Store  │ │
│  │ :8080        │    │ :4222              │ │
│  └──────┬───────┘    └────────────────────┘ │
│         │                                    │
│   ┌─────▼──────┐                             │
│   │ SQLite     │                             │
│   │ (1 ファイル)│                             │
│   └────────────┘                             │
└─────────────────────────────────────────────┘
```

**サイジング目安 (3000 台規模)**:

| リソース | 推奨 |
|---|---|
| CPU | 4 vCPU |
| RAM | 8 GB |
| Disk | 100 GB SSD |
| OS | Linux (Ubuntu 24.04 / Rocky Linux 9) **または Windows Server 2022 / 2025** |

### 2.10.2 HA 構成 (Phase 3)

```
[LB (HAProxy/nginx)]
    │
    ├─> [Backend #1]  ─┐
    └─> [Backend #2]  ─┤
                       │
                  ┌────▼──────────────┐
                  │ NATS Cluster (3)  │
                  │ + JetStream R=3   │
                  └───────────────────┘
                       │
                  ┌────▼──────┐
                  │ Postgres  │ (Primary + Standby)
                  └───────────┘
```

### 2.10.3 サーバー側インストール / サービス登録

サーバー側 (backend / nats-server) は systemd または Windows Service として常駐させる。両 OS で同じ Rust バイナリが動作する。

#### Linux (systemd)

```ini
# /etc/systemd/system/mgmt-backend.service
[Unit]
Description=Endpoint Management Backend
After=network.target nats.service

[Service]
ExecStart=/usr/local/bin/mgmt-backend --config /etc/mgmt/backend.toml
Restart=always
User=mgmt
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/nats.service
[Unit]
Description=NATS Server
After=network.target

[Service]
ExecStart=/usr/local/bin/nats-server -c /etc/nats/nats.conf
Restart=always
User=nats

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now nats.service mgmt-backend.service
```

#### Windows Server (Windows Service)

**Backend のサービス登録** (Rust の `windows-service` crate 経由でサービス対応バイナリにしておく):

```powershell
sc.exe create MgmtBackend `
  binPath= "`"C:\Program Files\Mgmt\mgmt-backend.exe`" --config `"C:\ProgramData\Mgmt\backend.toml`"" `
  start= auto `
  obj= LocalSystem `
  DisplayName= "Endpoint Management Backend"

sc.exe description MgmtBackend "Endpoint Management 1-binary backend (axum + SPA + Scheduler + Projector)"
sc.exe failure MgmtBackend reset= 86400 actions= restart/60000/restart/60000/restart/60000
sc.exe start MgmtBackend
```

**NATS Server のサービス登録** (NATS は標準で Windows Service 対応):

```powershell
# NATS バイナリは Windows 用が公式配布あり
# https://github.com/nats-io/nats-server/releases から nats-server-windows-amd64.zip 取得

# サービスとしてインストール
nats-server.exe `
  --config "C:\ProgramData\Nats\nats.conf" `
  --service install `
  --display "NATS Server" `
  --user_name "LocalSystem"

# 起動
sc.exe start nats-server
```

または NSSM (Non-Sucking Service Manager) を使ってもよい。Windows 環境では NSSM がデファクト:

```powershell
choco install nssm   # or scoop install nssm
nssm install MgmtBackend "C:\Program Files\Mgmt\mgmt-backend.exe"
nssm set MgmtBackend AppParameters "--config C:\ProgramData\Mgmt\backend.toml"
nssm set MgmtBackend Start SERVICE_AUTO_START
nssm start MgmtBackend
```

**Windows Server での注意点:**
- **ファイアウォール**: `New-NetFirewallRule` で TCP 4222 (NATS) と 8080 (Backend API) を許可
  ```powershell
  New-NetFirewallRule -DisplayName "NATS" -Direction Inbound -Protocol TCP -LocalPort 4222 -Action Allow
  New-NetFirewallRule -DisplayName "Mgmt API" -Direction Inbound -Protocol TCP -LocalPort 8080 -Action Allow
  ```
- **イベントログ**: backend / agent から `tracing-windows-eventlog` で Windows イベントログにも出力すると、運用チームが普段使用している Windows 監視ツールで検知できます
- **長いパスのサポート**: `LongPathsEnabled` レジストリを有効化しておく (JetStream の data ディレクトリが深くなりがち)
  ```powershell
  Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem" -Name LongPathsEnabled -Value 1
  ```
- **Defender 除外**: JetStream の data dir と Agent の install dir を除外推奨 (パフォーマンス対策)
  ```powershell
  Add-MpPreference -ExclusionPath "C:\ProgramData\Mgmt", "C:\ProgramData\Nats"
  ```

### 2.10.4 Agent インストール

```powershell
# サービス登録
sc.exe create MgmtAgent `
  binPath= "C:\Program Files\Mgmt\agent.exe" `
  start= auto `
  obj= LocalSystem `
  DisplayName= "Endpoint Management Agent"

sc.exe start MgmtAgent
```

配布は MSI パッケージ化 (WiX 等) または PowerShell + SCCM/GPO 経由。

### 2.10.5 Agent 自己アップデート

1. オペレータが `kanade agent publish <binary> --version <v>` を実行 → Object Store `agent_releases` に v 名でアップロード
2. 同コマンドが `agent_config.global.target_version` フィールドを `<v>` に書き換え (Sprint 6 の層化対応: per-group / per-pc 上書きで canary rollout も可能)
3. 各 Agent の `config_supervisor` が `agent_config` を watch、resolver で自分の `EffectiveConfig` を再計算 → `target_version` が `AGENT_VERSION` 定数と異なれば self_update タスクが発火
4. Object Store `agent_releases.<v>` からダウンロード → SHA-256 検証
5. **Atomic swap** (Plan A、v0.1.5):
   - staged blob を `<exe>.new` として exe と同一ディレクトリにコピー (= Program Files 内、cross-volume safe)
   - `<exe>` → `<exe>.old` を rename (atomic、Windows は loaded PE を delete 不可だが rename は可能)
   - `<exe>.new` → `<exe>` を rename (atomic、同一ディレクトリ内)
6. プロセスが `std::process::exit(64)` で抜ける → SCM が **failure-actions** (`sc.exe failure ... actions= restart/5000/restart/15000/restart/60000` + `sc.exe failureflag <svc> 1`) に従って新バイナリで再起動
7. 新プロセス起動時に `<exe>.old` を掃除 (`main.rs::cleanup_stale_upgrade_artifacts`)

deploy-agent.ps1 が初回登録時に `sc.exe failure` + `sc.exe failureflag` を設定するため、operator は self-update のために追加作業は不要。

3000 台展開ではこの仕組みを最初から実装することが事実上必須。

> [!NOTE]
> スクリプト（`scripts/fleet-deploy.ps1`, `deploy/linux/fleet-deploy.sh` 等）によるフリートデプロイからの脱却、全 operator OS 共通の `kanade deploy` CLI、型付き操作（Typed Native Operations）、および成果物 identity・安全な更新契約の全体設計については [§2.13 クロスプラットフォーム デプロイ設計](#213-クロスプラットフォーム-デプロイ設計-kanade-deploy-cli--typed-operations) を参照。

## 2.11 リポジトリ・ディレクトリ構成

### 2.11.1 ソースコード Workspace (Rust)

```
mgmt-system/                     # Cargo workspace
├── Cargo.toml                   # workspace 定義
├── crates/
│   ├── shared/                  # 共有型 (Rust ↔ Rust)
│   │   └── src/lib.rs           # Command, Inventory, Event, etc.
│   ├── agent/                   # Windows Agent
│   │   └── src/main.rs
│   ├── backend/                 # Backend サービス
│   │   ├── src/main.rs
│   │   ├── api/                 # HTTP API
│   │   ├── scheduler/           # 定刻発火
│   │   ├── projector/           # Stream → SQLite
│   │   └── worker/              # 結果回収
│   └── mgmtctl/                 # Admin CLI
│       └── src/main.rs
└── web/                         # フロントエンド
    ├── package.json
    ├── src/
    └── dist/                    # rust-embed が取り込む
```

### 2.11.2 運用設定リポジトリ (Git)

```
mgmt-repo/                       # 別 Git リポジトリ (GitOps)
├── jobs/                        # ジョブ定義
│   ├── cleanup-disk-temp.yaml
│   ├── windows-update.yaml
│   └── inventory-detail.yaml
├── scripts/                     # スクリプト本体
│   ├── windows-update.ps1
│   └── cleanup-disk-temp.ps1
├── groups/                      # 端末グループ定義
│   ├── canary.yaml
│   ├── wave1.yaml
│   └── wave2.yaml
└── schedules/                   # 定期実行定義
    └── daily-inventory.yaml
```

### 2.11.3 Agent 配置 (Windows)

```
C:\Program Files\Mgmt\
└── agent.exe                    # 実行バイナリ

C:\ProgramData\Mgmt\
├── agent.toml                   # 設定
├── outbox/                      # 未送信メッセージ (必要時)
├── state.db                     # ローカル状態 (SQLite)
└── logs\
    └── agent.log
```

### 2.11.4 Backend 配置 (Linux)

```
/usr/local/bin/
└── mgmt-backend                 # 実行バイナリ (SPA 同梱)

/etc/mgmt/
└── backend.toml

/var/lib/mgmt/
├── backend.db                   # SQLite
└── nats/                        # JetStream data dir

/var/log/mgmt/
└── backend.log
```

### 2.11.5 Backend 配置 (Windows Server)

```
C:\Program Files\Mgmt\
├── mgmt-backend.exe             # Backend バイナリ (SPA 同梱)
└── nats-server.exe              # NATS バイナリ

C:\ProgramData\Mgmt\
├── backend.toml                 # Backend 設定 (teravars 風)
├── backend.db                   # SQLite
├── certs\                       # mTLS 用証明書
│   ├── ca.crt
│   ├── server.crt
│   └── server.key
└── nats\
    ├── nats.conf                # NATS 設定
    └── jetstream\               # JetStream data dir

C:\ProgramData\Mgmt\logs\
├── backend.log
└── nats-server.log
```

**Windows Server 配置のポイント:**
- 実行ファイルは `Program Files`、データ・設定は `ProgramData` に置く (Windows のお作法)
- `ProgramData` は全ユーザ共通、`LocalSystem` 権限でアクセス可
- バックアップ対象は `C:\ProgramData\Mgmt\` 配下を丸ごとで OK

### 2.11.6 Client App 配置 (Windows)

本節の配置・配布・自己アップデートは **Windows 専用の旧設計記録**。MSI / ActiveSetup / `client_releases` を実装済みとはしない。独立 Client updater 案は §2.13 の全 OS 共通 jobs / app-packages 契約で置換済み。Linux / macOS の GUI 要件は §2.11.7 を参照。

```
C:\Program Files\Kanade\
└── kanade-client.exe           # Tauri バイナリ (WebView2 ランタイムは OS 既定)

%APPDATA%\Kanade\               # = C:\Users\<user>\AppData\Roaming\Kanade
└── client.toml                 # ユーザー設定 (言語、テーマ、起動時挙動等)

%LOCALAPPDATA%\Kanade\          # = C:\Users\<user>\AppData\Local\Kanade
├── logs\
│   └── client.log
└── cache\                      # 通知の既読状態キャッシュ等 (Agent KV が source of truth)

# 自動起動レジストリ
HKCU\Software\Microsoft\Windows\CurrentVersion\Run
└── KanadeClient = "C:\Program Files\Kanade\kanade-client.exe" --tray
```

**配布**: MSI パッケージ (WiX) で `Per-Machine` インストール + **ActiveSetup** で各ユーザー初回ログオン時にショートカット + Run キーを展開。Agent と同じ MSI に同梱しても、別 MSI に分けてもよい。

**自己アップデート**: Tauri の updater は使わず、**Agent の self-update と同じ Object Store 経路** (`client_releases` bucket) で配布する。Client は KLP で「自分のバージョン更新が必要か」 を Agent に問い合わせ、Agent (`LocalSystem`) が新バイナリを Object Store からダウンロード → 起動中の `kanade-client.exe` を停止 → `C:\Program Files\Kanade\` 配下を swap → 再起動。Swap 自体は `Program Files` への書き込みで特権が必要だが、これは **Agent が自身の権限で実施するため、エンドユーザーへの UAC 昇格要求は発生しない**。

### 2.11.7 Client App 配置 (Linux / macOS) — 設計決定記録

本節は Linux / macOS の Client App の設計決定記録。運用者が承認済みの決定 (**確定**) と、実装タスクが検証・具体化する **proposal（提案）** を区別する。いずれも未実装である (本変更は docs のみで、コード・workflow・ジョブ定義は変更しない)。KLP transport / identity / 接続所有の確定事項 (§2.12、§2.12.4.4)、現行 Windows の挙動、既存 Agent の配布・更新、ユーザーデータの分離、通知 click と acknowledgement の分離、tray 不在時の挙動、保守的な GUI session 適格判定は変更しない。
フリートデプロイ CLI (`kanade deploy`) および型付き Agent 操作（Typed Native Operations）による Client App の導入・更新・パッケージング契約については、[§2.13 クロスプラットフォーム デプロイ設計](#213-クロスプラットフォーム-デプロイ設計-kanade-deploy-cli--typed-operations) を参照。

**確定した決定（運用者承認済み）**

1. **対応環境**: 初期 macOS は Apple Silicon (aarch64)・macOS 14 以降。初期 Linux の検証・サポートは Ubuntu 24.04 LTS・x86_64・GNOME で、Wayland と X11 の両方。Debian、Fedora、KDE、Linux ARM、その他の環境は **延期** であり、サポート対象として告知しない (§2.11.7.8)。
2. **ログイン起動**: Client の導入で、GUI にログインする全ユーザーに非表示の `--tray` 起動が既定で有効になる。ユーザーの opt-out は更新をまたいで尊重する。SSH / headless session では起動しない。明示 Quit は通常の常駐を止めるが、緊急通知の fallback は Client を再起動し得る。この挙動はユーザー向けに文書化する (§2.11.7.4)。
3. **macOS 署名**: 初期の管理 / 試験用 Mac 配備は ad-hoc 署名。自己署名証明書の維持はしない。有償の Developer ID 署名と notarization は延期。ad-hoc 署名は Gatekeeper の配布信頼を確立しないため、アプリ単位の承認を文書化し、対応 Mac で実際の起動・通知・更新後の identity を検証する。Gatekeeper の全体無効化や、シームレスな信頼配布の主張はしない (§2.11.7.9)。
4. **導入・更新**: 既存の Kanade ジョブと app-packages 配布機構で行う (`install-kanade-client.ps1` に倣う)。macOS ジョブは pkg installer、Linux ジョブは apt / dnf を呼び、初期実装は Ubuntu / apt。独立した自動 updater、`client_releases` bucket、専用 watch / target loop、並行する更新所有者は初期に設けない。外部管理の既存 package を黙って引き継がない (§2.11.7.9)。

上記以外の本節の記述は、各小節で「確定」と明記したものを除き proposal であり、未解決の技術事項は各小節に明示する。owner は残る proposal の採否を確認する。実装タスクは確定事項と受入条件を引き継ぐ。

#### 2.11.7.1 現状と Windows 仕様との差

指定実装を基準にした現状は以下。§2.10.4 / §2.11.6 の MSI / ActiveSetup / 更新経路は設計であり、Unix へそのまま移植できる完成品ではない。

- この棚卸しは Unix GUI 着手前の記録。現在の `crates/kanade-client/src/main.rs` / `Cargo.toml` / `build.rs` は Windows と Apple Silicon macOS の GUI を持ち、Linux と Intel Mac は exit-fast shim。Mac の実 GUI / ad-hoc `.app` と、未整備の pkg / session integration は区別する (§2.13.3)。KLP ライブラリと `window_size.rs` の純粋ロジックは OS 共通である。
- `src/app.rs` は通常起動でウィンドウ表示、`--show-notification <id>` / `--resurface` で非表示起動、protocol クリックで表示・通知へ focus。single-instance が二回目の argv を転送する。tray 作成、`--tray` 判定、HKCU Run 登録はまだ無い。サイズは Tauri `app_local_data_dir()/window-size.json` に保存し、close は保存するだけで hide-to-tray にしていない。
- 通常通知は Tauri notification plugin、緊急通知は WinRT の `scenario=reminder` と protocol activation。AppUserModelID は Tauri identifier と Agent の Start Menu shortcut の `System.AppUserModel.ID` に一致させる。これは Windows 固有である。
- 現行の `configs/jobs/installers/scripts/install-kanade-client.ps1` は app-packages HTTP 経由で exe を取得し、operator 指定 SHA-256 を検証して Program Files に配置、HKLM に `kanade-client://` を登録する。Start Menu shortcut / 表示名の同期は `client_shortcut.rs` が担当する。MSI / ActiveSetup と §2.11.6 の HKCU Run 展開を実装済みとはしない。
- §2.11.6 のユーザー別 log は仕様上の配置。現行 `main.rs` は `C:\ProgramData\Kanade\logs\client.log` に書く。以下の表は **Windows 仕様との対応** であって、現行の log / Tauri path をそのまま写した表ではない。
- `session_supervisor.rs` は WTS の active console に idle 計測用 `--session-agent` を維持する Windows 専用機構。緊急通知 fallback は別の `klp/emergency_notify.rs` が WTS 経由で Client を起動し、10 秒 cooldown と logon / unlock 時の `--resurface` を持つ。Unix の通知配送に idle 計測まで同時移植する必要はない。
- `self_update.rs` が実装するのは `agent_releases` の Agent バイナリ検証・swap・exit(64)。`client_releases` は Windows 向け §2.11.6 の旧案であり、現行 crates に bucket 定義・publish / watch / Client 更新処理は無い。§2.13 で全 OS とも既存の app-packages 配布 + Kanade ジョブへ統一する (§2.11.7.9)。
- macOS の `deploy/macos/README.md` / `setup-agent.sh` は Apple Silicon の Agent を `/usr/local/bin/kanade-agent`、root LaunchDaemon を `/Library/LaunchDaemons/com.kanade.agent.plist` に配置する。現行 Agent の self-signed identity / quarantine 除去は GUI 配布の信頼モデルに流用しない。Linux の `setup-agent.sh` / systemd unit も root Agent を `/usr/local/bin` に配置する tarball 配布であり、Client package は無い。Linux README の旧 arch 記述より現行 release matrix を基準にする。

#### 2.11.7.2 配布・インストールの所有者

**proposal（提案）**: Client は任意導入の **per-machine、Agent と別成果物**。導入・更新は **確定** の通り既存の Kanade ジョブ (app-packages 配布、§2.11.7.9) が OS installer を実行する。Agent が独自に更新する経路は設けない。Client はユーザー権限で動き、NATS credentials を持たない。Agent-only / headless 端末に GUI dependency を追加しない。Client package は Agent の存在を確認するが、Agent 自体の tarball 配布・self-update を package に取り込まない。

| OS | proposal の配布・配置 | ファイルの所有者 |
|---|---|---|
| macOS | ad-hoc 署名 (確定、§2.11.7.9) の `Kanade Client.app` を `.pkg` で `/Applications/Kanade Client.app` に配置。固定 bundle identifier は現行 Tauri identifier を維持。Client の LaunchAgent は `/Library/LaunchAgents/com.kanade.client.plist` (通知用 session helper は別 plist) | root:wheel の bundle / plist、installer receipt。ユーザーによる bundle 内変更は不可 |
| Linux | 初期は Ubuntu 24.04 / x86_64 の `.deb` (`.rpm` と他 distro / arch は延期)。executable は `/usr/bin/kanade-client`、asset は `/usr/share/kanade-client/`、application entry は `/usr/share/applications/kanade-client.desktop`。user unit と XDG autostart entry も package 所有 | dpkg database (rpm は延期)。ジョブは更新実行者であって `/usr/bin` を直接 swap しない |

**代替案と却下理由**: Agent 同梱の単一 bundle は headless 導入に WebView / desktop 依存を持ち込むため採らない。`~/Applications` / `~/.local/bin` の per-user install はユーザー単位の重複更新と root Agent の管理対象識別が増えるため初期対象外。`/opt` の tarball / 生ファイル swap は運用が軽いが、署名 bundle、依存解決、package database、アンインストールの一貫性を優先して採らない。AppImage は試用には便利でも FUSE / desktop 統合 / 更新所有者が別になるため初期の管理対象外。Flatpak / Snap は sandbox と KLP socket / Agent 特権更新の契約を増やすため別検討。

**受入条件**: Agent-only install では GUI library を要求しない。新規・既存ユーザーがアプリ一覧から起動でき、uninstall は管理対象 bundle / package と起動登録だけを除去する。ユーザーデータは保持し、削除は別の明示操作とする。

#### 2.11.7.3 ユーザー別 config / log / cache

**proposal（提案）**: Agent の `/etc/kanade` / `/var/lib/kanade-agent` / `/var/log/kanade` と Client データを分離する。Linux は絶対パスの XDG 変数を尊重し、未設定・空・相対パスは表の既定値へ戻す。home は対象ユーザーの account 情報から解決し、root Agent の HOME を継承しない。

| 用途 | Windows 仕様 (§2.11.6) | Linux proposal (XDG 既定値) | macOS proposal |
|---|---|---|---|
| 設定 | `%APPDATA%/Kanade/client.toml` | `$XDG_CONFIG_HOME/kanade-client/client.toml` (`~/.config/...`) | `~/Library/Application Support/Kanade/Client/client.toml` |
| log | `%LOCALAPPDATA%/Kanade/logs/client.log` | `$XDG_STATE_HOME/kanade-client/logs/client.log` (`~/.local/state/...`) | `~/Library/Logs/Kanade/Client/client.log` |
| cache / WebView data | `%LOCALAPPDATA%/Kanade/cache/` | `$XDG_CACHE_HOME/kanade-client/` (`~/.cache/...`) | `~/Library/Caches/Kanade/Client/` |
| 永続 UI state | 現行は Tauri app-local data の `window-size.json` | `$XDG_STATE_HOME/kanade-client/window-size.json` | `~/Library/Application Support/Kanade/Client/window-size.json` |

Client が自身の UID で directory 0700 / file 0600 を作る。log は日次 rotation、7 日保持を proposal とし、他ユーザーの通知本文を共有 log に混ぜない。cache 消失は許容し、既読状態は Agent 側を source of truth として `notifications.list` から復元する。window-size は cache clear で消さない。WebView の実保存先もこの policy に合わせる (Tauri の path resolver 任せで別の場所へ散在させない)。

**代替案と却下理由**: Agent と同じ共有 log はアクセス権とユーザー分離が合わない。全データを Tauri identifier 配下に任せる案は簡単だが、config / state / cache を分ける運用要件を満たさない。サイズを cache に置く案は清掃のたびに UX が変わるため採らない。Windows 既存パスの是正は別タスクであり、本節は移行済みとはしない。

**受入条件**: XDG override・非 ASCII home・書込不能・壊れた state・cache 削除でも起動できる。二 UID の設定・log・既読表示が混ざらない。保存失敗は log に記録し GUI を停止しない。

#### 2.11.7.4 ログイン起動と GUI session 境界

**確定**: Client の導入で、GUI にログインする全ユーザーに `--tray` の非表示起動が既定で有効になる。ユーザーが無効化 (opt-out) した状態は更新・再導入をまたいで尊重する。SSH / headless session では起動しない。明示 Quit はその session での通常の常駐を止めるが、緊急通知の fallback が Client を再起動し得る。**ユーザー向けの文書にこの挙動 (Quit しても緊急通知で再起動され得ること) を明記する**。通常の手動起動と deep link は表示する。

**proposal（提案）**: 既存 / 新規ユーザーに machine-wide 登録が効くようにし、各 home を root で巡回・改変する ActiveSetup 風処理は作らない。opt-out の保存場所と検出方法 (OS のログイン項目の状態を読めるか、Client 側の設定にするか) は **未解決** で、実装タスクで OS ごとに検証する。以下の LaunchAgent / systemd の構成はその前提の proposal。

- **macOS proposal**: package 所有の LaunchAgent に `LimitLoadToSessionType=Aqua`、`RunAtLoad=true`、固定 `Contents/MacOS/kanade-client --tray` を設定。LaunchDaemon から GUI を起動しない。`KeepAlive` は付けず、Quit 後の即再起動を避ける。既存 login session は管理対象 plist を `gui/<uid>` に bootstrap、未ログイン user は次回 Aqua login でロードする。通知用の軽量 session helper は別の `/Library/LaunchAgents/com.kanade.client-session.plist` から Aqua 内で起動し、Client の Quit と独立して login / lock 状態を監視する。helper の再起動は backoff 付きで、UI を起動する根拠にはしない。アンインストール時はこの二つの管理対象 job のみ bootout。
- **Linux proposal**: `/etc/xdg/autostart/kanade-client.desktop` を GUI からの入口とし、root 所有の session launcher が確認した環境で systemd user の `kanade-client.service` (`ExecStart=/usr/bin/kanade-client --tray`) を start。unit は `PartOf=graphical-session.target`、`Restart=no` とし、default.target に enable しない。session launcher は GUI target を管理する対応 desktop でのみ使い、session launcher は軽量 helper として GUI session 内に留まり、Client が Quit しても session 状態を登録・監視する。logout で helper / Client と登録環境を破棄する。systemd の XDG generator を使う desktop でも二つの Client service を作らず入口を一本化する。[systemd の desktop 統合](https://github.com/systemd/systemd/blob/main/docs/DESKTOP_ENVIRONMENTS.md) に従って対象 desktop ごとに検証する。

**代替案と却下理由**: macOS Login Items / SMAppService は OS の統合 UI に利点があるが、初期案では machine-wide package と Agent の session 起動を同じ plist で管理する。Linux の XDG entry から Client を直接常駐させる案は単純だが、停止・更新・GUI 環境の寿命を unit に揃えるため採らない。systemd user manager の存在、lingering、SSH login、`DISPLAY=:0` の推測は GUI 在席の証拠にならない。systemd / logind の無い desktop は初期対応外 (手動起動と履歴閲覧は可能)。

**受入条件**: 既存 / 新規の全 GUI user が login 時に `--tray` で非表示起動し、opt-out が更新後も保持されることを確認する。Quit 後に通常の再起動 loop が無く、緊急 fallback では再起動され得ることがユーザー向けに説明されている。GUI logout / SSH-only / lingering では Client を起動しない。自動起動を無効化してもアプリ一覧から起動できる。

#### 2.11.7.5 緊急通知を正しいユーザーへ配送する

**proposal（提案）**: 通知用の `GuiSessionResolver` / launcher を idle sampler、KLP peer identity とは別に設ける。KLP の UID 認可は §2.12.4.4 のまま。GUI session 情報は表示可否にのみ使う。root Agent 自身から OS toast を送らない。既存 KLP の session fan-out / 履歴取得は維持し、OS toast と自動起動だけに表示可否を適用する。SSH から起動した Client の購読者がいるだけで fallback を抑制しない。session helper の登録・presence 配送は新しい内部契約として後続タスクで定義し、peer credentials / OS process 所属を検証する。KLP の認可用 session を追加する契約ではない。

共通契約は以下を proposal とする。

1. 現行通知の宛先は all / PC / group であり user_key 宛てではない (既読が user_key 単位)。自動 fallback は現在の console / seat0 の **local / active / unlocked GUI** の UID に限定し、起動直前に再確認する。本文取得・既読状態はその UID の KLP 契約に従う。裏 session へ自動起動せず、別 UID の既読や session 情報を渡さない。
2. 起動中なら既存 instance に通知 id を配送、終了していれば固定 executable をそのユーザーとして `--show-notification <id>` で非表示起動。単一起動の単位は UID + 適格 GUI session。argv は shell 展開せず、本文・command・任意 executable を受け取らない。
3. launch cooldown は Windows と同じ **10 秒** を UID / session ごとに持つ。抑制分は pending として残し、cooldown 終了時に適格 session を再確認して `--resurface` する。live push と fallback の同じ id は重複表示を抑える。
4. lock / logout / session 不明なら表示・起動を控え、次の確認済み login / unlock で `--resurface`。Client は `notifications.list` で未読・未失効・toast 対象を再取得する。pending はヒントであり Agent restart 後も履歴から回復する。toast click は表示・focus のみ、`notifications.ack` は明示的な確認操作だけ。
5. headless / SSH / 失敗時は通知データを失わず履歴へ劣化する (§2.12.4.4)。launch 失敗は非致命的な理由付き log、無限 retry / GUI 強制切替はしない。

**macOS proposal**: SCDynamicStore console user (loginwindow / root を除外) と `gui/<uid>` bootstrap domain の存在を両方確認する。Aqua の独立 session helper が NSWorkspace の session active / inactive と、distributed notification の `com.apple.screenIsLocked` / `com.apple.screenIsUnlocked` を監視する案とする。後者は公開された安定 API 契約ではないため対応 OS ごとの互換性試験を必須とし、通知欠落・helper restart・初期状態不明では unlocked を推測しない (確認済み unlock まで自動 toast を控え、手動の履歴閲覧へ落とす)。定期的に console / bootstrap / helper の生存を照合し、情報の鮮度が不明なら表示しない。console UID の一致だけで SSH / fast user switching の裏 session を適格にしない。Agent は `launchctl asuser <uid>` の bootstrap context に入り、さらに supplementary groups / GID / UID を対象ユーザーへ落として固定 bundle executable を実行する。`asuser` 単独は資格情報の降格ではない (`man launchctl` を実装時にも確認)。HOME / USER / TMPDIR は対象ユーザー用に設定し root の環境は渡さない。LaunchAgent job が管理中ならその job を起動し、追加 argv は同じ session の単一起動配送へ繋ぐ。

**Linux proposal**: system bus の logind で `Type=x11|wayland`、`Class=user`、`Remote=false`、active seat0、対象 UID を確認する。[logind の属性](https://github.com/systemd/systemd/blob/main/man/org.freedesktop.login1.xml) の `LockedHint` は desktop が設定する hint なので、未対応 desktop / lock 状態不明を unlocked と決めつけない。XDG session launcher の登録した session id / pid / UID と OS の session 所属を照合し、その session の `XDG_RUNTIME_DIR`、user bus、DISPLAY / XAUTHORITY または WAYLAND_DISPLAY を使用する。登録は KLP UID と OS の pid 情報で検証し、ユーザー入力の shell / 任意環境変数を root 実行へ渡さない。Agent は対象 UID に降格して user unit を start し、固定 executable への二回目の launch で id を転送する。同じ UID の複数 GUI は user bus / manager を共有し得るため初期は自動配送を控え、複数 seat / remote GUI も履歴へ落とす。古い user manager 環境は流用しない。GNOME (初期対象。KDE は延期) の helper は desktop の ScreenSaver D-Bus 状態 / 変化も照合し、logind の LockedHint と矛盾・取得不能なら unknown とする。

**代替案と却下理由**: macOS の `open -a ... --args` だけでは稼働 instance への引数配送を保証できないため Agent fallback の主経路にはしない (OS deep link 起動は別経路)。Linux の `/proc` 環境の無差別コピー、`sudo -u` 単独、root の D-Bus 通知は session / bus の取り違えを招く。全 GUI に fallback executable を起動する案は多重表示を増やすため採らない (KLP の既存 fan-out は維持)。macOS の private な session dictionary key を読み unlocked と断定する案は互換性と初期状態の保証が弱いため採らない。helper を Client UI 内だけに置く案は Quit 中の lock / unlock を観測できない。Windows と同様に lock 中も toast を送る案は Unix のロック画面への露出を制御しにくいため初期案では控える。

**受入条件**: Client 終了中の緊急通知、同じ通知の連続送信、10 秒以内の unlock、Agent restart、fast user switching、二 UID、同一 UID の複数 GUI、SSH / headless で誤配送しない。session helper の停止・古い環境・lock 判定不能でも履歴から回復する。macOS の lock helper は通知責務だけを持ち、KLP `session=None` を変更しない。

#### 2.11.7.6 OS 通知と persistent の劣化

**proposal（提案）**: `NotificationAdapter` に表示、capability、クリックの id 配送を集約する。WebView から WinRT に直接依存する現行経路を OS adapter に分け、Windows の reminder / AppUserModelID の挙動は維持する。通知許可拒否、DND / Focus、server 不在も成功した ack とは扱わない。

| OS | proposal の API / crate 候補 | click と persistent |
|---|---|---|
| macOS | `UNUserNotificationCenter` + response delegate。Rust bridge は `objc2-user-notifications` / `objc2-foundation` 等を候補にする (依存採用は後続タスク) | notification identifier / userInfo に id、通常 click / 確認 action を共通 focus 処理へ。冷起動時も delegate を早期登録して初期応答を保持。許可は初回の可視起動で説明して要求し、背景起動から要求 dialog を出さない |
| Linux | user session bus の `org.freedesktop.Notifications`、`zbus` / `notify-rust` を候補にする。action / signal の寿命を制御できる adapter を選ぶ | `GetCapabilities` を確認、default / 確認 action と `ActionInvoked` を id に対応付ける。persistent は `expire_timeout=0` と対応 server の resident hint を要求するが保証しない |

macOS の応答は [UNUserNotificationCenterDelegate](https://developer.apple.com/documentation/usernotifications/unusernotificationcenterdelegate) を使う。persistent は Notification Center への通常通知に劣化し、banner / alert の選択、消去、Focus は OS / ユーザーに従う。timeSensitive / critical alert、全画面 dialog、繰返し音を永続表示の代用品にはしない。Linux の無期限要求・resident は [freedesktop 仕様](https://specifications.freedesktop.org/notification/latest-single/) の capability / server policy に従う。action 不在なら click-to-open を保証せずアプリ一覧から履歴を開く。Wayland の focus は action の activation token が取得できれば使用し、compositor が拒否すれば表示して注意表示に留める。

close-to-hide では Client が生存するので Linux action signal を処理できる。明示 Quit / crash 後は D-Bus sender の action 対応が消える可能性があり、既存 toast click による冷起動は保証しない。履歴と次の緊急 fallback が回復経路となる。macOS は OS が bundle を再起動する通知応答を受入試験で確認し、GUI 不適格時は本文を表示せず復帰を待つ。

**代替案と却下理由**: Tauri notification plugin を全 OS の唯一の API にする案は通常 info / warn には便利だが、緊急通知の action・冷起動・persistent の制御を必要とするため主経路には採らない。plugin の将来版で同じ契約を満たすと実証できれば adapter 内で置換可能。Linux `notify-send` subprocess は action の受信寿命を管理しにくい。persistent を「必ず消えない」とする案は OS に保証できないため採らない。

**受入条件**: 許可あり / 拒否 / 未決定、DND、actions 非対応、server restart、cold / warm click、Quit 後の劣化を確認する。click / dismiss / 自然消去では ack が変わらず、履歴表示を失わない。

#### 2.11.7.7 Deep link・単一起動・tray・サイズ

**proposal（提案）**: 共通 URI は現行の `kanade-client://show?id=<id>` (`show/?id=` も互換) を維持し、起動時 argv、macOS URL-open event、稼働時 single-instance、OS notification response を一つの `focus_notification(id)` に接続する。macOS の URL event は argv だけでは届かないため Tauri deep-link plugin を候補とする ([Tauri deep linking](https://v2.tauri.app/plugin/deep-linking/))。plugin の採用・single-instance 連携は後続実装であり現行設定には追加されていない。

- **macOS proposal**: 署名前の `Info.plist` に `CFBundleURLTypes` / `CFBundleURLSchemes=kanade-client` を記述し LaunchServices 登録へ。bundle id / executable / URL scheme は更新で維持し、実行時に plist を書換えない。
- **Linux proposal**: application `.desktop` に `MimeType=x-scheme-handler/kanade-client;`、固定 `Exec=/usr/bin/kanade-client %u` を登録し desktop database を更新。autostart entry は別ファイル。既存のユーザー default handler は root から上書きせず、未設定の登録と競合時の案内を受入対象にする。
- **共通 proposal**: scheme / route / query を厳密検証、id は `[A-Za-z0-9_.-]` の全体一致、1–128 byte、URI 全体は 2048 byte 以下、重複 id / 未知 query / 不正 escape は拒否する。現行 Windows parser の「許可文字まで切り出す」挙動より厳しくする案であり実装済みとはしない。URI から job / ack / file / command を実行しない。id は KLP で対象 UID の履歴を再取得して確認、存在しない・アクセス不可なら通知一覧に戻す。初期 WebView ready / KLP reconnect まで focus 要求を保持する。

**proposal（提案）**: tray menu は「開く」「通知」「終了」。macOS は template icon の menu bar item とし、通常可視起動は Dock / アプリ一覧からも復帰できる。Linux は Tauri tray / StatusNotifierItem (AppIndicator) を使うが、GNOME の extension が無い環境は tray 不在を正常な劣化とする。`--tray` の非表示契約は維持し、tray が無いだけでウィンドウを自動表示しない。アプリ一覧、deep link、次の緊急通知が復帰経路になる。

close はサイズ保存後に hide、明示 Quit は終了。起動時は **サイズだけ** を logical pixel で復元し min / work area に clamp、不正ファイルは既定値とする。位置・最大化・最小化・前回 visibility は保存せず、`--tray` / `--show-notification` / `--resurface` が visibility に優先する。二回目の `--tray` は既存 window を表示しない。ユーザー起動 / click だけが show / unminimize / focus を要求する。

**代替案と却下理由**: GNOME extension の必須導入は管理対象外の desktop 設定を増やす。tray 不在で自動表示する案はログイン / 緊急通知で画面を奪わない契約に反する。window-state plugin で位置や visibility を丸ごと復元する案は monitor / DPI / Wayland の差と非表示起動の衝突を増やす。launcher / 通知 sender の名称を Agent が bundle / package 内で動的変更する案は署名・package 所有に反するため採らず、OS 側は固定製品名、KLP の `client_display_name` は Client window / UI 内で反映する。

**受入条件**: URL の冷起動・稼働中・二 UID・不正入力、tray 有無、Dock 再表示、close / Quit、monitor / DPI 変更、resize 直後の終了でも多重 instance や意図しない表示が起きない。

#### 2.11.7.8 WebView と対応 OS / build matrix

**確定**: 初期 macOS は **Apple Silicon (aarch64)、macOS 14 以降**。WKWebView は OS の framework を使用し、別 runtime installer を持たない。これは製品の検証下限であって Tauri の理論上の下限とは別。Intel / universal bundle は既存 Agent の arch 契約と揃わず初期対象外、古い OS の広範な対応は CI / 署名 / WebView の試験負担から採らない。

**確定**: 初期 Linux の検証・サポートは **Ubuntu 24.04 LTS、x86_64、GNOME、Wayland と X11 の両方** (systemd + logind、local session)。Debian、Fedora (rpm / dnf)、KDE、Linux ARM (aarch64)、その他の distro / desktop / 非 systemd / Alpine は **延期** で、サポート対象として告知しない。後継 release も自動で対応済みとはせず追加試験する。**proposal**: musl ではなく **GNU (glibc)** の Client をビルドする。既存 Linux musl Agent / Backend の target は維持し、GUI build を workspace の musl 一括 build に混ぜない。

Tauri 2 の Linux 要件は **WebKit2GTK API / ABI 4.1** ([Tauri prerequisites](https://v2.tauri.app/start/prerequisites/))。`4.1` は WebKit engine の実体バージョンではない。engine **2.40 以上**を proposal の下限とし、実装時に採用する wry / webkit2gtk の symbol 要件が上なら引き上げる。OS の security update は適用し、2.40 に固定しない。runtime は Ubuntu の deb `libwebkit2gtk-4.1-0` (rpm の `webkit2gtk4.1` は Fedora 延期のため参考)、build は対応する `-dev` / `-devel` と GTK3 / libsoup3 / compiler / pkg-config 等を用意する。GTK / tray library / fonts も package dependency に含め、実際の linkage から distro 別の依存を検証する。対象 package の存在は [Ubuntu 24.04](https://packages.ubuntu.com/noble/libwebkit2gtk-4.1-0) で確認できるが、package の存在だけで GUI 動作を保証しない。

**proposal（提案）**: 後続 CI は Windows 回帰、macOS aarch64 の GUI build / bundle、Ubuntu 24.04 x86_64 の GNU GUI build / install smoke を別 job にする。Linux は Ubuntu 24.04 の build root で作り glibc 下限を固定する。Tauri runtime / build-helper の cfg を拡張し、WinRT / ShellExecute は Windows adapter に残す。SPA embed と既定 `custom-protocol`、bundle version 同期も Unix build で確認し、Vite server の無い production package を起動する。

**代替案と却下理由**: WebView を同梱する案は security patch の責任と容量を増やす。AppImage / musl で全 distro を一つにする案は GTK / glibc / WebKit の ABI 問題を解決しない。対応 OS を Tauri の下限へ無条件に広げる案は製品として検証できない。現在の release matrix は Linux musl 2 arch / Windows / macOS aarch64 のバイナリ archive であり、Unix GUI package を提供しているとはしない。本変更では CI / Cargo / Tauri 設定を編集しない。

**受入条件**: 最低 OS / arch の clean image に package dependency と、macOS では文書化したアプリ単位の承認手順を前提に導入・起動可能。WebKit 不足は installer 段階で検出し、headless Agent install は引き続き成功。実際の artifact が shim でないことと SPA embed を検証する。

#### 2.11.7.9 Kanade ジョブによる Client 導入・更新・復旧

**確定（運用者承認済み・未実装）**: Unix の Client 導入・更新は、Windows の `configs/jobs/installers/scripts/install-kanade-client.ps1` (`install-kanade-client` ジョブ) と同じく、**既存の Kanade ジョブ** と **app-packages 配布機構** (現行は `GET /api/app-packages/kanade-client/<version>`、名前 + version による取得、operator 指定 SHA-256、必要時の Bearer token。新 deploy は §2.13.4 の identity / publisher 検証へ段階移行) だけで行う。更新の所有者は常にこのジョブであり、次は初期段階で追加しない。

- Client 専用の自動 updater、`client_releases` Object Store bucket、専用の watch / target loop、更新 journal、Agent / Client の KLP 更新状態契約。
- ジョブと並走する第二の更新経路 (並行する更新所有者)。Tauri updater は使わない。
- 既存 Agent の配布・self-update の挙動変更。Windows の `install-kanade-client` ジョブと exe 配置も本節だけでは変更しない。§2.13 の typed adapter への移行は parity gate 後に行う。

OS 別の installer 呼び出しは以下で確定。

| OS | ジョブが呼ぶ installer | 初期の扱い |
|---|---|---|
| macOS | `installer -pkg <検証済み .pkg> -target /` | 実装対象 |
| Linux | `apt` / `dnf` の local package transaction (distro に応じて選択) | **Ubuntu 24.04 / `apt` のみ初期の実装・サポート対象**。`dnf` 経路は選択の枠組みだけを設計に残し、Fedora 等は §2.11.7.8 の通り延期 |

OS package manager を迂回した生ファイル上書きや rollback はしない。app-packages の名前と version は空でなく slash を含められない。同名・同版の再 upload は可能な既存契約であり、immutable artifact を仮定しない。取得 bytes は毎回 operator 指定 SHA-256 で検証する。

**ジョブの契約 (既存 Windows ジョブの流儀に揃える。ただし以下の具体形は proposal)**:

1. **停止前の取得と検証**: app-packages から `<version>` の package を一時 staging へ取得し、operator が manifest / script に指定した SHA-256 と照合する。未指定なら取得前に失敗する。不一致は staging を破棄して現行 install に触れない。SHA-256 の pin は取得物の完全性を示すだけで publisher の信頼にはならない。これに加え、停止前に package のメタ情報 (macOS は bundle id / version / arch、Ubuntu は `Package` / `Version` / `Architecture`)、OS / arch 下限、空き容量、依存解決の事前確認 (apt は simulate 等) を行う。確認項目の最終形は実装タスクで定める。
2. **版と管理元の検出**: 導入済み版を OS の正本から読む (macOS は installer receipt と bundle の `Info.plist`、Ubuntu は dpkg database)。同一版なら no-op とし、結果に現行版を返す。管理元は Kanade ジョブ由来か外部管理かを判定する。判定方法は未解決 (下記)。
3. **外部管理の非奪取**: 外部の配布系 (別の apt repository / 端末管理製品 / 手動導入) が管理していると判定または判定不能な既存 package は、**黙って引き継がず、変更せずに拒否して結果を報告する**。管理移管は operator の明示操作によるものとし、ジョブが暗黙に行わない。
4. **制御された停止**: 管理対象の固定 install path / bundle id と OS の process identity (実行ファイル path と UID) で照合した Client instance だけを対象にする。名前一致の `pkill` は使わない。新規 launch を一時抑制し、終了要求、猶予後に残る対象のみ停止。対象を確認できなければ install せず、延期として報告する。
5. **導入**: 上表の installer を呼ぶ。package manager の lock 競合は上限付きで待ち、超過は延期として報告する。user data (§2.11.7.3) には触れない。
6. **検証と復帰**: 導入後に receipt / package database と実ファイル (bundle の `Info.plist` 等) から版を読み戻し、要求版だけを成功版として報告しない。実行ファイルの形式 (shim ではないこと) も確認する。再起動は **更新前に稼働していた現在も適格な GUI session** (§2.11.7.5 の保守的判定) の Client に限り、UID / session を再確認して `--tray` で戻す。logout 済み、SSH、headless、更新前に Quit 済みの Client は起動せず、前回の visibility や通知 click も復元しない。更新中に届いた通知は履歴から回復する。復帰対象の適格な GUI session がある場合は実際の起動、KLP 再接続、実稼働版を確認し、GUI 不在では GUI 検証未実施と job result に明記する。
7. **有界実行**: ジョブ全体は manifest の `timeout:` で上限を持つ。download / lock 待ち / 停止猶予 / 導入 / 検証への配分は未解決 (下記)。timeout で中断し得るため、手順は再実行しても安全であることを要件とする。
8. **報告**: 既存のジョブ結果 (exit code、stderr の進捗、stdout の単一 inventory JSON) で報告する。stdout は inventory projector が parse する JSON 一行のみとし、進捗は stderr に出す。inventory は少なくとも `version` と配置先を返す。新しい KLP メッセージやバックエンド側の更新状態テーブルは追加しない。
9. **失敗時の復旧**: 失敗は原因 (検証失敗 / 延期 / install 失敗 / 検証失敗) とともに job result で報告する。復旧は **operator が既存ジョブを旧版の version / SHA-256 で再実行する** ことを候補とする。macOS の `.pkg` 再 install と apt の明示 downgrade は非原子的な処理であり、途中中断や電源断で中間状態が残り得る。自動 rollback は保証しない。復旧失敗時は要修復を報告し、同一 target を自動で retry し続けない。

**macOS の署名 (確定)**: 初期の管理 / 試験用 Mac 配備は **ad-hoc 署名** とし、自己署名証明書の維持はしない。Developer ID 署名と notarization (有償) は延期する。ad-hoc 署名は Gatekeeper の配布信頼を確立しない。したがって:

- アプリ単位で必要な承認 (初回起動時の許可操作等) を運用手順として文書化する。Gatekeeper を全体として無効化しない。シームレスな信頼配布を謳わない。quarantine 属性の除去を信頼の代用にしない。
- 対応 Mac (macOS 14 以降 / Apple Silicon) の実機で、新規導入と更新後の **起動、通知の表示、アプリの identity (通知許可や LaunchServices の紐付けが更新後も保たれること)** を検証する。この検証が済むまで macOS 配備を完了とは扱わない。
- 既存 Agent の self-signed 配布の信頼モデルは GUI 配布に流用しない。

**Linux の管理元 (確定部分)**: 初期は Ubuntu 24.04 の apt のみ。外部管理の package は上記 3 の通り引き継がない。

**未解決の技術事項 (製品決定は行わない)**:

- 管理元の識別方法 (Kanade ジョブが導入した印の持たせ方、外部 apt repository / 端末管理製品の判別)。
- 外部管理拒否の結果区分 (失敗 / スキップ / 延期のいずれとして job result に表すか)。
- package の命名と、OS / arch / format の区別 (現行 endpoint は name + version のみ。名前に埋め込むか別の取り決めか、ならびに同名・同版の再 upload が可能な契約下での運用)。
- pkg 自体の署名の有無と、ad-hoc 署名下での bundle 完全性の検証手段 (`codesign --verify` 等) の範囲。
- ad-hoc 署名の bundle での通知許可と identity (designated requirement) が更新をまたいで保たれるか、保たれない場合の運用。
- macOS で必要なアプリ単位の承認の具体的な操作と、管理 Mac への事前承認の可否。
- opt-out の保存場所と検出方法、および更新・再導入をまたいで尊重する方法 (§2.11.7.4)。
- ジョブ (root / system) から対象ユーザー session へ Client を戻す仕組み。§2.11.7.5 の GUI 在席判定と launcher をそのまま再利用できるか。
- 更新中に緊急通知 fallback が Client を起動する場合との競合 (起動抑制の仕組みと排他)。
- Client に正常終了を要求する経路 (KLP に該当 method は無い。signal 等の OS 手段で足りるか)。
- 有界実行の時間配分と、中断後の再実行で旧版を保持する方法 (旧 package の保持場所、取得不能時の扱い)。
- 閉域環境の依存解決 (承認済み mirror / 事前配布)。外部 repo の利用可能性は仮定しない。

**代替案と却下理由**: Agent 起点の独立 updater / `client_releases` / 専用 target loop は、既存ジョブ・app-packages・承認 (`require_approval`) と更新所有者が二重になるため初期は採らない。Windows の exe swap を Unix に写す案は、macOS の署名 bundle と Linux の package 所有を壊す。ユーザー権限の Tauri updater は per-machine install と権限経路が分裂する。macOS で自己署名証明書を維持する案は、鍵管理の負担に対して Gatekeeper の信頼が得られないため初期は採らない。全端末に version hold を強制する案は外部管理者の方針を上書きする。

**受入条件**: 次を注入または確認できる。hash 不一致、違う arch / OS、package 不在、依存不足、disk full、package manager lock、Client 停止失敗、install 中の中断・電源断、検証失敗、旧版再実行による復旧の成否。開始・延期・拒否・成功・失敗が既存の job result と inventory で operator に見える。管理対象外のアプリ、外部管理 package、user data に触れない。macOS は ad-hoc 署名の実機検証 (起動・通知・更新後の identity) が完了している。Windows と Agent の配布・更新が回帰しない。

#### 2.11.7.10 承認後の実装順と CI / 実機受入

**proposal（提案）**: KLP transport は既存実装を利用し再実装しない。以下を小さな独立タスクに切り、各 adapter / OS の失敗が他 OS を止めない境界を先に作る。番号は推奨順、依存番号を満たした OS の作業は別々に進められる。CI は計画であって現在の workflow を変更済みとはしない。

| # | タスク / 依存 | CI で検証する範囲 | 実機 / VM の受入条件 |
|---|---|---|---|
| 1 | Client paths / 起動 mode / GUI eligibility の純粋 policy と adapter 境界 | XDG fallback、UID 分離、lock / stale / ambiguous session、引数、size の単体試験 | config / log / cache の権限と非 ASCII home |
| 2 | macOS GUI build / WKWebView と bundle version (1) | aarch64 GUI compile、SPA embed、plist / version 検査 | 最低 macOS で shim ではない GUI 起動 |
| 3 | Linux GNU GUI build / WebKit dependency (1)。Ubuntu 24.04 x86_64 | Ubuntu 24.04 build root、Xvfb smoke、headless Agent 回帰 | GNOME の X11 / Wayland 表示と font |
| 4 | URI parser / focus queue / UID-session 単一起動、OS 登録 (2 / 3、OS 別) | malformed URI、cold / warm event、二 instance、plist / desktop 検査 | LaunchServices / default handler、二 UID、Wayland focus |
| 5 | tray / --tray / close / Quit と size 配線 (4) | mode とサイズの回帰、GUI smoke | macOS menu bar / Dock、GNOME tray 不在、DPI / monitor 切替 |
| 6 | macOS notification adapter (4) | fake notification center / delegate、id / ack 分離 | 許可 dialog、拒否、Focus、終了後の実 click |
| 7 | Linux notification adapter (4) | private D-Bus fake server、capability / action / timeout / server loss | GNOME の実 click、persistent 劣化、Wayland token |
| 8 | macOS LaunchAgent / session resolver / emergency fallback (5 / 6) | fake console / launchctl runner、UID 降格引数、cooldown / resurface | Aqua login、lock / unlock、fast user switching、SSH-only |
| 9 | Linux XDG / user unit / logind session 配送 (5 / 7) | fake logind + user bus、環境 whitelist、session race、cooldown | logout、lingering、SSH-only、二 UID / 同一 UID 複数 GUI |
| 10 | macOS `.app` / `.pkg` の構築と ad-hoc 署名 (8) | pkg 構造試験、ad-hoc 署名の `codesign --verify`、plist / version 検査 | clean Mac (macOS 14 / Apple Silicon) での導入・既存 / 新規 login・uninstall |
| 11 | Ubuntu 24.04 `.deb` の構築、install / uninstall と ownership (9) | clean Ubuntu VM / container の dependency・dpkg database 検査 | desktop 登録、閉域依存、既存 / 新規 login |
| 12 | macOS の job ベース導入・更新・復旧 (10。既存 Kanade ジョブ + app-packages、`installer -pkg`) | fake installer / launchctl runner による検証前処理・停止対象照合・外部管理拒否・timeout・stdout JSON の試験 | 実 installer transaction、中断・電源断、旧版再実行による復旧、適格 session だけの再起動 |
| 13 | Ubuntu / apt の job ベース導入・更新・復旧 (11。同ジョブ機構) | fake package runner による lock・依存不足・外部管理拒否・timeout・再実行の試験 | 実 apt transaction、閉域依存、外部管理との競合、旧版再実行、適格 session だけの再起動 |
| 14 | ad-hoc 署名の実機検証と運用手順 (10 / 12) | 手順文書の確認 (署名資格情報は不要) | 対応 Mac で新規導入・更新後の起動、通知、identity、アプリ単位の承認手順を確認。Gatekeeper を無効化しない |

`dnf` / Debian / Fedora / KDE / Linux ARM、Developer ID 署名と notarization は延期であり、上表にタスクを置かない。独立 updater、`client_releases`、専用 watch / target loop、更新 journal のタスクは置かない (§2.11.7.9)。上表の 12–14 は後続の実装タスクで、本変更では実装も enqueue もしない。

**代替案と却下理由**: 全 OS の通知・配布・更新を一つの大型タスクにすると session 配送や署名の未検証を transport 完了に紛れ込ませる。署名の実機検証 (ad-hoc 署名の起動・通知・identity) を hosted CI だけで済ませる案は採らず、構造 / fake の PR CI と実機検証を分ける。hosted CI の green だけでは通知許可・実 click・ロック解除・desktop extension 不在・Wayland foreground を保証できず、表の実機試験完了まで対応済みとはしない。

## 2.12 KLP (Kanade Local Protocol)

Client App ⇄ Agent の IPC プロトコル。OS の認証機構をそのまま使い、追加の鍵管理を不要にする。「**エンドユーザーに NATS credentials を渡さない**」 を確実に守るための層。

**OS 間の共通性**: Framing (§2.12.2)、JSON-RPC (§2.12.3)、Method 名前空間 (§2.12.5)、Handshake (§2.12.6)、Error Model (§2.12.9) は **OS 非依存で Windows / Linux / macOS 共通**。OS ごとに異なるのは Transport の実体 (§2.12.1) と、接続元 identity の取得方法 (§2.12.4) のみ。

**実装状況**: Agent 側は Windows (Named Pipe)・Linux・macOS (UDS) とも実装済み (`crates/kanade-agent/src/klp`)。Unix のソケットパスは環境変数 `KANADE_KLP_SOCKET_PATH` で上書きできる (テスト・開発用)。Client の KLP ライブラリも 3 OS 対応済み (`crates/kanade-client/src/klp_client.rs`)。Tauri GUI の Linux / macOS 対応は後続。

### 2.12.1 Transport

| OS | エンドポイント | Agent の動作形態 |
|---|---|---|
| Windows | Named Pipe `\\.\pipe\kanade-agent` | `LocalSystem` の Windows Service |
| Linux | Unix Domain Socket `/run/kanade/agent.sock` | root の systemd service。ディレクトリは unit の `RuntimeDirectory=kanade` (mode 0755) で作成 |
| macOS | Unix Domain Socket `/var/run/kanade/agent.sock` | root の launchd daemon (`com.kanade.agent`、`deploy/macos/README.md` 参照) |

**ACL / 認可**:
- Windows: Pipe security descriptor を `Authenticated Users` に RW、`Everyone` / `Anonymous` は拒否 (SDDL `D:(D;;GA;;;AN)(A;;GA;;;AU)`)
- Linux / macOS: **ソケット mode `0666`、親ディレクトリ mode `0755` (root 所有)**。ファイルシステム権限では絞らず、**認可は Agent が connect 時に peer credentials から行う** (§2.12.4.1)。意図は Windows の「Authenticated Users RW」と同じ (= 認証済みの通常ユーザーなら接続できる)。

**グループ方式 (`0660` + `kanade-users`) を採らない理由**:
- macOS には全ユーザーに自然に付与される共有グループがなく、Linux と同じ規則にできない。
- グループ所属は端末ごと・ユーザーごとの配布作業になり、ユーザー追加・ドメイン/ディレクトリ連携のたびに運用負担が発生する。
- OS ごとに ACL の意味が割れるより、「ソケットは広く開け、Agent が peer を判定する」で 3 OS の意図を揃えるほうが仕様として単純。

**トレードオフ**: ファイルシステム権限という防御層を捨てて Agent の判定に集約する。判定のバグ = 防御層ゼロとなるため、拒否条件は §2.12.4.1 で数値・規則として固定し、テスト可能な契約とする。

Agent は **1 listener、複数同時接続** を受け付ける。対象は Windows の Fast User Switching / RDP の同時セッション、Linux の複数ユーザー (複数ログイン)、macOS の fast user switching。接続ごとに peer identity を取得するため、同時に複数ユーザーが接続しても互いの状態 (購読・ack・unlock) は混ざらない。

### 2.12.2 Framing

- **Length-prefix**: 4-byte little-endian `u32` (本体のバイト長)
- **Body**: UTF-8 JSON 文字列
- 最大メッセージサイズ: 1 MiB (`stdout_chunk` は分割すること)

```
[len: 4 bytes LE u32] [body: len bytes UTF-8 JSON]
[len: 4 bytes LE u32] [body: len bytes UTF-8 JSON]
...
```

### 2.12.3 Protocol

**JSON-RPC 2.0** ([spec](https://www.jsonrpc.org/specification))。3 種類のメッセージ:

| 種別 | 形状 | 用途 |
|---|---|---|
| Request | `{jsonrpc, id, method, params}` | Client → Agent、応答待ち |
| Response | `{jsonrpc, id, result \| error}` | Agent → Client、Request への応答 |
| Notification | `{jsonrpc, method, params}` (id なし) | 双方向 push (server push 含む) |

Request の `id` は Client 採番 (**UUID v7 推奨** — 時系列ソート可、ログとの相関容易)。

### 2.12.4 Authentication / Authorization

#### 2.12.4.1 Peer identity の取得と接続認可

Connect 時に Agent が **OS から接続元 identity を取得**し、platform-neutral な `PeerIdentity` を作る。

| 項目 | Windows | Linux | macOS |
|---|---|---|---|
| user (所有権キー) | SID | UID (`SO_PEERCRED`) | UID (`LOCAL_PEERCRED`) |
| pid | `GetNamedPipeClientProcessId()` | `SO_PEERCRED` | `LOCAL_PEERPID` |
| session | `OpenProcessToken()` → `GetTokenInformation(TokenSessionId)` | peer pid の logind session (`sd_pid_get_session`)、取得不能なら `None` (§2.12.4.4) | `None` (§2.12.4.4) |

**PeerIdentity** (論理形。実装は `klp/auth.rs` の OS 別 cfg で埋める):

```
PeerIdentity {
  user_key:     String,        // 認可・状態キー。Windows: SID 文字列 / Unix: "uid-<UID>" (NATS KV キー・subject に使える文字のみ)
  user_display: String,        // 表示・ログ用。Windows: "DOMAIN\\user" / Unix: ユーザー名 (引けなければ "<unknown>")
  pid:          u32,           // OS 上の接続元の特定・session 照合専用。所有権キーには使わない
  session:      Option<u32>,   // OS の session 識別子。Unix では取れないことがある
}
```

- **Payload に user_id を入れない**。Agent は OS 由来の identity を真とみなす (この原則は 3 OS 共通で維持)。
- `user_key` は改名に耐える安定キーとする (Windows の SID と同じ理由で、Unix ではユーザー名でなく UID)。`user_display` は表示専用で認可に使わない。
- **リスク**: Unix では UID が再利用されうる (ユーザー削除後に同じ UID を別人に割り当てる等)。その場合、既読 (`notifications_read`) や unlock グラントが別人に引き継がれうる。Windows の SID は再利用されない点で非対称。運用上は UID を再利用しない前提とし、後続実装で影響を再評価する。

**接続認可 (Linux / macOS)**: Agent は accept 直後、**要求を 1 バイトも読む前に** peer を判定し、次のいずれかに当てはまる peer を拒否する。

1. UID が下限未満 — Linux は `UID < 1000`、macOS は `UID < 500` (システムアカウント)
2. UID が 0 (root)。例外は設けない (root 用の操作は KLP を経由せず Agent/CLI 側で行う)
3. passwd エントリ (`getpwuid`) が存在しない
4. ログイン shell が空、または絶対パスでない、`/etc/shells` に未登録、あるいは basename が `nologin` / `false` (非対話アカウント)
5. 資格情報・アカウント情報の取得に失敗した (**fail closed**)

Windows は SD (`Authenticated Users`) が同等のゲートで、追加の Agent 側拒否規則は置かない。

**拒否時の応答**: 拒否した peer には、`id: null` の Error 応答を 1 フレームだけ書いて close する。以降は一切 read しない (0666 では未認可の peer が誰でも接続できるため、拒否した相手からは読まないほうが資源枯渇に強い)。

```jsonc
{"jsonrpc":"2.0","id":null,"error":{
  "code": -32000, "message": "Unauthorized",
  "data": {"kind":"Unauthorized","retryable":false,"detail":"uid 998 is below the minimum interactive uid 1000"}
}}
```

`detail` には拒否理由 (上記 1〜5 のどれか) を入れる。さらに `data.retryable` (bool) で拒否の性質を区別する。

| 規則 | 性質 | `data.retryable` |
|---|---|---|
| 1 (UID 下限) / 2 (root) / 4 (非対話 shell) | 恒久。アカウント自体が不適格 | `false` |
| 3 (passwd エントリなし) / 5 (取得失敗) | 一時的でありうる。sssd / LDAP / opendirectoryd の再起動や短時間のディレクトリ障害で起きる (Agent の self-update 直後の再接続など) | `true` |

規則 3・5 は fail closed (接続は拒否する) が、恒久拒否とは扱わない。Client の扱いは §2.12.10。

**後続実装での制限事項 (記録)**:
- 認可前の接続は誰でも張れるため、認可前接続の同時数と持続時間 (判定は即時なので短い timeout) を後続実装で制限する。
- UID の下限 (`UID_MIN` の変更、MDM 管理下の特殊アカウント等) は環境により例外が出うる。設定での上書きは今回は決めない。

#### 2.12.4.2 現行実装で peer の Session ID が使われている箇所 (調査結果)

Windows 実装を調べた結果、peer の Session ID は**認可にも所有権にも使われていない**。

- 使用箇所は handshake 応答の `HandshakeSession.session_id` (u32) と、`server.rs` / `klp_client.rs` のログ出力のみ。
- 所有・状態の判定:
  - `jobs.kill`: 接続ごとの `runs` 集合 (`Connection::owns_run`) で判定
  - `notifications.ack` / `unack`、既読一覧、`support.unlock` / `lock` / `status`、`jobs.list` の解錠状態: peer の SID (`user_sid`) で判定
  - 通知の配信先: pc / group / all で決まり、購読中の全接続に fan-out する (Session では絞らない)
- toast 起動 (`emergency_notify`) と `session_supervisor` が対象にするのは、peer の Session ではなく `WTSGetActiveConsoleSessionId()` の**アクティブなコンソール session** (在席・ロック状態を含む)。KLP 接続の identity とは独立。
- 実装 (注記): `notifications.ack` / `unack` は、SID と通知 ID の検証後に、通知が自分宛 (pc / 自分の所属 group / all) かを配信と同じ `filter_subjects` の対象 subject で確認する。宛先外の通知と未知 ID はどちらも `Unauthorized` を返す (他宛ての存在を探れないようにするため)。宛先内の通知の再 ack は冪等。保持期限切れや purge 済みの通知は ack / unack できない。

→ **Linux / macOS の等価物が提供すべきもの**: (a) 安定した user_key (= UID)、(b) 接続単位の識別 (pid・接続オブジェクト)、(c) 任意の session 情報 (表示・ログ用)。Session の厳密な等価物は認可上は必須ではない。toast / session_supervisor 相当の「GUI session の在席・切替・ロック状態」は KLP 接続の identity とは別責務として後続実装で扱う (§2.12.4.4)。

#### 2.12.4.3 Method ごとの認可と、依存する identity

| Method | 強制する規則 | 依存する identity |
|---|---|---|
| `jobs.execute` | manifest に `user_invokable: true` 必須 (false なら `IpcError::Unauthorized`) | `user_key` (監査ログの actor) |
| `jobs.kill` | **自分の接続で投げた `run_id` のみ** kill 可 (それ以外は `Unauthorized`)。cancel は agent プロセス内の kill registry へ直接伝えるため broker に依存せず、agent は `kill.*` を publish しない (broker 越しの `kill.{exec_id}` による remote kill は backend API / CLI 用に従来どおり)。結果は run の終端 `jobs.progress` (status = Killed) で届く。終了済み・未知の run への kill は no-op | **接続** (`Connection::runs`)。user_key は関与しない |
| `notifications.ack` / `unack` / 既読一覧 | 自分宛 (`pc` / 自分の所属 `group` / `all`) のみ ack 可。既読状態は `user_key` ごとに独立 | `user_key` |
| `support.unlock` / `lock` / `status`、`jobs.list` の解錠状態 | 解錠グラントは `user_key` ごとに保持 | `user_key` |
| `system.handshake` 応答 / ログ | 表示・相関のみ | `user_display`、`session`、`pid` |

- レート制限: 1 接続あたり 60 req/min を超えたら `-32003 RateLimit`

#### 2.12.4.4 確定事項 (同一ユーザーの別接続・Session の等価物)

**(1) 同一ユーザーの別接続による kill / ack の扱い — 決定: 現行の「自接続限定」を維持**
- `jobs.kill` は引き続き run を開始した接続のみとする。run は実行中の副作用を持つため、同じ UID の別プロセスに run を乗っ取られる余地を作らないことを最優先する。
- `notifications.ack` / `unack` と `support.unlock` / `lock` は引き続き `user_key` 単位とし、同じ `user_key` の別接続から更新できる (再起動・多重起動で状態が引き継がれる)。
- 既知の制約: Client の再起動や自動再接続のあと、実行中の run を `jobs.list` / `jobs.progress` で見ることはできても kill できない。将来緩める場合は「同一 `user_key` **かつ** 開始元接続がすでに切れている」を条件とする後続検討であり、現在の許可条件ではない (「同一 `user_key` なら可」とはしない)。

**(2) Session の等価物 — 決定: 任意情報のみ、認可には使わない**
- Session は認可に使われていない (§2.12.4.2) ため、等価物がなくても Linux / macOS の KLP は成立する。session は任意の表示・ログ用情報であり、認可には一切使わない。
- Linux: peer pid の logind session (`sd_pid_get_session`) が取得できればそれを保持し、取得できなければ `None` とする。
- macOS: `None` とする。SCDynamicStore の console user は UID の一致しか示さず、同じ UID の SSH 接続や fast user switching の裏セッションを区別できないため、console 一致や audit session id による判定は誤判定のリスクから採らない。
- session が無い (SSH / ヘッドレス等) ことを理由に接続は拒否しない。これは資格情報 (UID 等) の取得失敗による拒否規則とは別物であり、適格なアカウントなら KLP を許可し、GUI の不在を認可の拒否理由にしない。
- toast / `session_supervisor` に相当する機能は、peer の Session とは別の「**GUI session の在席・切替・ロック状態**」責務として分離し、後続で扱う。Linux は logind のアクティブなグラフィカル session、macOS は SCDynamicStore の console user を基準にする。GUI が確認できなければ toast を出さず、通知は `notifications.list` の履歴から回復する。

### 2.12.5 Method 名前空間 (v1)

| Method | 種別 | 用途 |
|---|---|---|
| `system.handshake` | req-rep | プロトコルバージョン交渉 (接続後 1 回目に必ず呼ぶ) |
| `system.ping` | req-rep | 死活 |
| `system.version` | req-rep | Agent + Client App バージョン |
| `system.log_tail` | req-rep | agent.log の末尾 N 行 (サポート問い合わせ用) |
| `state.snapshot` | req-rep | 端末ヘルス + inventory + コンプライアンスチェックの一括スナップショット |
| `state.subscribe` | req-rep | `state.changed` 購読開始 |
| `state.changed` | push (A→C) | health / version 等の状態変化 |
| `notifications.list` | req-rep | 過去通知一覧 (paginated, filter: unread/all) |
| `notifications.subscribe` | req-rep | `notifications.new` 購読開始 |
| `notifications.new` | push (A→C) | 新着通知 (emergency 含む) |
| `notifications.ack` | req-rep | 既読化 (Agent → NATS `events.notifications.acked.>` publish) |
| `jobs.list` | req-rep | `client:` ブロックを持つ manifest 一覧 (filter: category)。`client.visible_to` で pc/group 静的スコープ、`client.show_when` で check 結果による動的表示ゲート |
| `jobs.execute` | req-rep | 実行依頼。返り値は `run_id`。`visible_to` は再判定するが `show_when` / `unlock` は表示ゲートなので見ない |
| `jobs.subscribe` | req-rep | `jobs.progress` 購読開始 |
| `jobs.progress` | push (A→C) | stdout chunk / exit code / status 変化 |
| `jobs.kill` | req-rep | 自接続で投げた run の停止 (同一ユーザーの別接続からは不可、§2.12.4.4) |
| `support.upload_diagnostics` | req-rep | サポート問い合わせ用 zip を Object Store にアップロード |
| `support.unlock` | req-rep | サポートコードを照合し `client.unlock` スコープを時限表示 (listing のみ) |
| `support.lock` | req-rep | 保持中の解錠グラントを即時破棄 |
| `support.status` | req-rep | 現在保持している解錠グラント一覧 (再接続時のバナー復元用) |
| `maintenance.list` | req-rep | 今後 N 日に予定された自端末向け job 一覧 |
| `maintenance.defer` | req-rep | 配信された再起動の延期申請 (15m/30m/1h) |

#### `client.show_when` — check 結果による動的表示ゲート

`client:` ジョブは `show_when: { check: <slug>, is: <status|[status…]> }` で、
**指定 check の最新結果が `is` のいずれかである間だけ** `jobs.list` に出る。
代表例は「アップデート済みなら更新ボタンを出さない」: 最新なら `ok` を返す
check を用意し、更新ジョブを `is: [fail]` でゲートする。

- **評価は Agent 側** `jobs.list` 時。`StateSnapshot.checks` を参照する。checks は
  **check 名でキー**されるため、検査用 `check:` ジョブと表示対象ジョブは別 manifest で
  よく、同じ slug を共有できる。
- **未実行 check (snapshot 不在) は不一致** → 検査が一度走るまでジョブは出ない
  (`visible_to` と同じ fail-closed)。
- `visible_to` が listing と `jobs.execute` の両方を縛る認可境界なのに対し、
  `show_when` は **listing のみ**の UX ゲート (execute は縛らない → list/execute レース無し)。
- **即時反映**: `jobs.execute` 経由の run も manifest の `check:` を実行・記録するため
  (`emit:` と違い stdout を消さない)、更新ジョブに `check:` を同梱して完了時に同名 check を
  `ok` で emit すれば、検査ジョブのスケジュールを待たず即フリップする。Client は run 完了
  (terminal `jobs.progress`) で `jobs.list` を再取得し、消えたジョブを反映する。
- **ゲート専用 check を Health から隠す**: `check.health: false`(既定 true、`fleet` と対称)で、
  その check を **Client App の Health タブに出さない**(健全性サマリのカウントからも除外)。
  ゲート駆動だけが目的の `myapp-up-to-date` のような検査がエンドユーザーの Health 画面を
  汚さない。check は引き続き `StateSnapshot.checks` に載るので show_when は動く;
  Client は wire の `Check.health_hidden`(agent が `!health` をセット)で描画時に除外する。
  `fleet`(SPA フリート集計軸)と `health`(Client Health 軸)は直交し、純粋なゲート検査は
  両方 off にすればどこにも出ずゲートだけ駆動する。

#### `client.unlock` — サポートコードによる時限表示 (裏コマンド)

`client:` ジョブは `unlock: <scope>` を持てる。そのジョブは通常
**`jobs.list` に出ない**。情シス / ヘルプデスクが問合せ対応中に
**Client App でサポートコードを入力** すると、そのスコープが一定時間だけ開き、
対象ジョブがカタログに現れる。

**これは表示ゲートであって認可境界ではない**(`show_when` と同系統、`visible_to` とは別系統)。
`jobs.execute` では再判定しない。理由と帰結:

- **見えているものは必ず実行できる**。listing と run の両方を縛ると
  「ボタンが見えている状態で TTL が切れ、押したら失敗する」レースが生まれる。
- 逆に **セキュリティ境界としては使えない**。標準ユーザーが Agent の Named Pipe に
  直接 KLP を話し、job id を知っていれば実行できる。特権作業の承認ゲートは
  operator (SPA) の exec 経路側に置く。ここはボタンを隠すだけ。

- **コードの保管**: `server_settings.support_codes[]` に **argon2id ハッシュのみ**
  (`{scope, hash, label, ttl_minutes, disabled}`)。平文はどこにも保存しない。
  設定は専用エンドポイント `PUT/DELETE /api/server-settings/support-codes/{scope}`
  (operator+)。**汎用の `PUT /api/server-settings` はこのフィールドを触らない** —
  API 応答はハッシュをマスクするので、フォームの往復で生きたコードを消せない構造にする。
- **Agent 向け projection**: `server_settings` には installer 用の NATS トークン
  (KV 上は平文) や SMTP などの operator secret も入っているため、Agent には読ませない。
  backend が `support_codes` だけを `fleet_config` バケットの `support_codes` キー
  (`{"support_codes":[{scope, hash, label, ttl_minutes, disabled}]}`) に書き出し、
  Agent の `support.unlock` はこれだけを読む。サポートコード API の成功後と backend
  起動時、および定期 (5 分) に `server_settings` の最新値から再導出する。コードを
  全消去しても**キーは削除せず空リストを書く** (キーの存在が「backend が reconcile
  済み」の印になる)。キーが無いとき**だけ** (旧 backend / 未 reconcile) 新 Agent は
  従来どおり `server_settings/current` を読む。読取・デコード失敗は fallback せず
  「確認できない」として返す。
- **`server_settings` の閉じ方**: 旧 Agent は `server_settings` を直接読むため、
  backend は文書をそのまま保つ。全 Agent を更新し、`fleet_config` に `support_codes`
  キーが存在することを確認したら、broker 側で Agent から `server_settings` への
  アクセスを全面 deny できる。これで installer の NATS トークン等が全 Agent から
  読めなくなる。照合は引き続きローカルなので、backend 停止中でも desk からの解錠は動く。
  それまでは (旧 Agent が読むため) deny してはならない。
- **照合は Agent 側 (ローカル)**。backend 停止中でも解錠できる (= 最も必要な場面)。
  KV は NATS トークン (HKLM の SYSTEM+Administrators ACL) がないと読めないため、
  標準ユーザーはハッシュにも到達できない。ローカル管理者は元より上位の権限を持つ
  (#1155) ので、本ゲートの防御対象は**標準ユーザー**であって端末管理者ではない。
- **グラントは OS ユーザー (SID) 単位 + TTL**。接続単位ではないので、Client の再接続
  (#468) でサポート中に黙って再ロックされない。期限は **monotonic clock** で判定
  (wire の `expires_at` は表示専用) — 時刻を巻き戻して延長できないため。
  Agent プロセス終了で全消滅 (fail-closed)。
- **失敗はレート制限**: SID 単位で 5 分内 5 回失敗 → 5 分ロックアウト。
  誤コード / 無効スコープ / コード未設定は**すべて同じ `Unauthorized`** を返す
  (スコープの存在を列挙させない)。
- **監査**: 成功・失敗とも `ObsEvent`(`source: agent:support`,
  `kind: support_unlock` / `support_unlock_failed` / `support_lock`) を per-PC
  タイムラインに残す。「誰の端末をいつ誰が開けたか」が運用上の主目的の半分。
- operator の `POST /api/exec` は `client:` を参照しないため無影響
  (`visible_to` / `confirm` と同じ方針)。

### 2.12.6 Handshake (接続後最初に必ず呼ぶ)

```jsonc
C→A {"jsonrpc":"2.0","id":"01931a8e-...","method":"system.handshake",
     "params":{"client":"kanade-client","client_version":"0.1.0",
               "protocol":[1], "features":["push.notifications","push.jobs","push.state"]}}

A→C {"jsonrpc":"2.0","id":"01931a8e-...",
     "result":{"protocol":1, "agent_version":"0.4.0",
               "features":["push.notifications","push.jobs","push.state","support.diagnostics"],
               "session":{"user":"DOMAIN\\alice","session_id":2,"pc_id":"PC1234"}}}
```

- `session.session_id` (u32): 表示・互換のためのフィールド。Windows は Session ID、Unix は UID を入れる (wire 型は 3 OS 共通で維持)。**認可キーでも実 session の識別子でもない**ため、Client UI は「セッション番号」として表示しない。Unix で実 session は §2.12.4.1 の `PeerIdentity.session` に別途保持し、wire には出さない
- `protocol`: Client が話せるバージョンの配列。Agent が話せる最大版を選んで `result.protocol` で返す。合意できなければ `-32004 StaleProtocol`
- `features`: optional method の availability。後方互換のため追加機能は **features bit** で表明する
- Handshake 未完了の状態で他 method を呼ぶと `-32600 InvalidRequest`

### 2.12.7 Subscription Lifecycle

```jsonc
// 購読開始
C→A {"jsonrpc":"2.0","id":"...","method":"notifications.subscribe"}
A→C {"jsonrpc":"2.0","id":"...","result":{"subscription":"sub-n-1"}}

// push (id なし notification)
A→C {"jsonrpc":"2.0","method":"notifications.new",
     "params":{"id":"notif-9f3a","priority":"emergency","require_ack":true,
               "title":"...","body":"...","issued_at":"..."}}

// 購読停止
C→A {"jsonrpc":"2.0","id":"...","method":"notifications.unsubscribe",
     "params":{"subscription":"sub-n-1"}}
A→C {"jsonrpc":"2.0","id":"...","result":null}
```

- 切断時は Agent 側で **その接続の全 subscription を自動解除**
- 再接続時は Client が再度 `subscribe` を呼ぶ。漏れた push は Agent 側の **未 ack KV (`notifications_read` の欠落)** から `notifications.list` で再構築できるので、push の at-most-once 保証で OK

### 2.12.8 完全な対話例 (緊急通知)

```jsonc
// 接続 → handshake
C→A {"jsonrpc":"2.0","id":"u1","method":"system.handshake",
     "params":{"client":"kanade-client","client_version":"0.1.0","protocol":[1],
               "features":["push.notifications","push.jobs"]}}
A→C {"jsonrpc":"2.0","id":"u1",
     "result":{"protocol":1,"agent_version":"0.4.0",
               "features":["push.notifications","push.jobs","push.state"],
               "session":{"user":"DOMAIN\\alice","session_id":2,"pc_id":"PC1234"}}}

// state.snapshot (起動時の一括取得)
C→A {"jsonrpc":"2.0","id":"u2","method":"state.snapshot"}
A→C {"jsonrpc":"2.0","id":"u2",
     "result":{"pc_id":"PC1234","online":true,
               "checks":[{"name":"bitlocker","status":"ok"},
                         {"name":"av_signature","status":"warn","detail":"3 日前"}],
               "agent_version":"0.4.0","target_version":"0.4.0"}}

// 通知 subscribe
C→A {"jsonrpc":"2.0","id":"u3","method":"notifications.subscribe"}
A→C {"jsonrpc":"2.0","id":"u3","result":{"subscription":"sub-n-1"}}

// 緊急通知が飛んでくる
A→C {"jsonrpc":"2.0","method":"notifications.new",
     "params":{"id":"notif-9f3a","priority":"emergency","require_ack":true,
               "title":"緊急: ネットワーク機器メンテ","body":"22時から30分停止します",
               "issued_at":"2026-05-20T12:00:00Z","issued_by":"infra-team"}}

// ユーザーが「確認」ボタンを押した
C→A {"jsonrpc":"2.0","id":"u4","method":"notifications.ack",
     "params":{"id":"notif-9f3a"}}
A→C {"jsonrpc":"2.0","id":"u4","result":{"acked_at":"2026-05-20T12:00:05Z"}}
// ↑ Agent は接続元の SID を OS から取得し、内部で
//    events.notifications.acked.PC1234.S-1-5-21-...-1001.notif-9f3a を NATS publish
//    (同じ PC の別ユーザーの ack と衝突しないよう {user_sid} を subject に含める)
```

### 2.12.9 Error Model

```jsonc
{"jsonrpc":"2.0","id":"u5","error":{
  "code": -32000,
  "message": "Job not user-invokable",
  "data": {"kind":"Unauthorized","detail":"manifest 'reboot' has user_invokable=false"}
}}
```

| Code | Kind | 意味 |
|---|---|---|
| -32700 | ParseError | JSON parse 失敗 |
| -32600 | InvalidRequest | JSON-RPC envelope が不正 / handshake 未完了 |
| -32601 | MethodNotFound | 未知 method |
| -32602 | InvalidParams | params の型 mismatch |
| -32603 | InternalError | Agent 側 panic / 想定外 |
| -32000 | Unauthorized | 認可エラー (user_invokable=false / 他人の run_id 等) |
| -32001 | NotFound | job_id / run_id / notif_id 不在 |
| -32002 | AgentDisconnected | Agent ↔ NATS が断、操作不可 |
| -32003 | RateLimit | 1 接続あたりの req/sec 超過 |
| -32004 | StaleProtocol | handshake で合意した version と非互換 |
| -32005 | PayloadTooLarge | 1 MiB 上限超過 |

### 2.12.10 Reconnection Policy (Client 側)

| 状況 | Client 側挙動 |
|---|---|
| 初回接続失敗 | exponential backoff (1s, 2s, 4s, ..., cap 30s) で再試行 |
| 接続中の切断 | 即座に再接続 → handshake → `state.snapshot` → 各種 `subscribe` を張り直す |
| Agent 起動前 (boot 時の race) | 上記 backoff で待つ |
| 接続直後に `id: null` の `-32000 Unauthorized` を受けた (§2.12.4.1 の拒否) | `data.retryable` が `false` (または欠落) なら**再接続しない**。認可条件を満たさないアカウントなので、再試行しても結果は変わらない。トレイ等で理由 (`detail`) を表示するに留める。`true` (passwd 引き失敗・資格情報取得失敗など一時的な拒否) なら通常の exponential backoff で再接続する |
| Pipe / Socket が存在しない | Agent service が停止中。トレイアイコンを ⚠️ 表示、「Agent サービスを開始してください」 案内 |

### 2.12.11 Schema 共有

KLP の全 method の params / result 型は `kanade-shared/src/ipc/` に Rust 構造体として置く:

```rust
// crates/kanade-shared/src/ipc/methods.rs
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Debug)]
#[ts(export, export_to = "ipc/")]
#[serde(rename_all = "snake_case")]
pub enum JobCategory {
    SoftwareUpdate,
    Troubleshoot,
    Catalog,
}

#[derive(Serialize, Deserialize, TS, Debug)]
#[ts(export, export_to = "ipc/")]
pub struct UserInvokableJob {
    pub id: String,
    pub display_name: String,
    pub display_description: Option<String>,
    pub icon: Option<String>,
    pub category: JobCategory,
    pub version: String,
    pub timeout_secs: Option<u64>,           // #865: manifest execute.timeout in secs; client watchdog honors it
    pub last_run: Option<JobRun>,
}

#[derive(Serialize, Deserialize, TS, Debug)]
#[ts(export, export_to = "ipc/")]
pub struct ExecuteJobParams {
    pub id: String,
}

#[derive(Serialize, Deserialize, TS, Debug)]
#[ts(export, export_to = "ipc/")]
pub struct JobProgress {
    pub run_id: String,
    pub status: RunStatus,                   // Queued | Running | Completed | Failed | Killed
    pub stdout_chunk: Option<String>,
    pub stderr_chunk: Option<String>,
    pub exit_code: Option<i32>,
}
```

`ts-rs` で `bindings/ipc/*.ts` に export → Tauri webview の TS から `import type` で参照。**Rust ⇄ Rust (Agent ⇄ Client backend) ⇄ TS (WebView) で単一スキーマ**。API ミスマッチが起こり得ない。

### 2.12.12 Observability

- Agent 側 `tracing` で `klp.request.{method}` span を出す
- Client 採番の `request_id` を span attribute に乗せる
- KLP 経由のリクエストを SQLite `audit_log` に **operator 起動と同じ扱いで記録** する (`actor = "user:DOMAIN\\alice@PC1234"`)

### 2.12.13 実装責務分担

| 責務 | 配置 |
|---|---|
| KLP listener (Pipe / UDS) | `crates/kanade-agent/src/klp/server/` (`windows.rs`: Named Pipe / `unix.rs`: UDS / 共通部は `mod.rs`) |
| `PeerIdentity` 定義と OS peer 認証 | `crates/kanade-agent/src/klp/auth/` (`mod.rs`: 型、`windows.rs`: `cfg(windows)`: Pipe → token / `cfg(target_os = "linux")`: `SO_PEERCRED` / `cfg(target_os = "macos")`: `LOCAL_PEERCRED` + `LOCAL_PEERPID`) |
| 接続認可 (UID 下限・非対話アカウント拒否) | Unix 側の `klp/auth/unix.rs`。Windows は `klp/security.rs` の SDDL |
| Pipe security descriptor | `crates/kanade-agent/src/klp/security.rs` (Windows) |
| 接続状態 (購読・自接続の run 集合) | `crates/kanade-agent/src/klp/connection.rs` |
| GUI session の在席・ロック検知 (toast 起動可否) | Windows: `klp/emergency_notify.rs` + `session_supervisor.rs`。Linux / macOS は後続実装 (§2.12.4.4) |
| Method ディスパッチ + ハンドラ | `crates/kanade-agent/src/klp/handlers/*.rs` |
| 共有型 (params / result / error) | `crates/kanade-shared/src/ipc/` (ts-rs export) |
| KLP client (Rust 側) | `crates/kanade-client/src/klp_client.rs` |
| Tauri command bridge | `crates/kanade-client/src-tauri/src/commands.rs` |
| WebView 側 (TS) | `crates/kanade-client/web/src/lib/klp.ts` |

---

## 付録: 実装ロードマップ

### Sprint 1 (最小動作)

- [ ] Cargo workspace 構造化、shared crate に型定義
- [ ] Agent: NATS 接続、自分宛 subscribe、echo back
- [ ] mgmtctl: 直接 NATS publish、結果表示
- [ ] NATS 単体構築、Subject 設計実装

### Sprint 2 (MVP 機能)

- [ ] Agent: 子プロセス実行 + kill signal 対応 (重要)
- [ ] Agent: 定期インベントリ採取 (WMI)
- [ ] JetStream Stream / KV 設計を実装
- [ ] バージョン照合 (script.current KV)

### Sprint 3 (Backend 化)

- [ ] axum で API サーバ実装
- [ ] SQLite + Projector
- [ ] YAML パーサ + 検証
- [ ] mgmtctl を API 経由に切り替え
- [ ] 監査ログ

### Sprint 4 (運用機能)

- [ ] Wave 配信、Jitter
- [ ] Scheduler (cron)
- [ ] Web UI (SPA + rust-embed 同梱)
- [ ] OIDC 認証
- [ ] Agent 自己アップデート

### Sprint 5 (v0.2.0): サーバ管理のグループメンバシップ — **完了**

- [x] `agent_groups` KV bucket + AgentGroups wire 型 (sort + dedup invariants)
- [x] Agent: KV watch + 動的 subscribe/unsubscribe マネージャ (純関数 diff + integration glue)
- [x] Backend admin API: `/api/agents/{pc_id}/groups` (GET/PUT/POST/DELETE)
- [x] CLI: `kanade agent groups [list|add|rm|set]`
- [x] `agent.toml::[agent] groups` を deprecate (`#[serde(default)]` で互換維持、v0.4.0 で削除予定)
- [x] backend が startup で `agent_groups` バケットを auto-bootstrap (v0.3.1)

### Sprint 6 (v0.3.0): 層化された agent_config — **完了**

- [x] `ConfigScope` / `EffectiveConfig` / `ResolutionWarning` wire 型 + 純関数 `resolve()` (built-in → global → groups.alphabetical-last-wins → pc)
- [x] Agent: `config_supervisor` タスクで `agent_config` + `agent_groups` 両 watch、`tokio::sync::watch` で配布
- [x] Heartbeat / inventory が動的 cadence 反映 (interval 入れ替え)、self_update が per-group / per-pc target_version 対応
- [x] Backend admin API: `/api/config`, `/api/groups/{n}/config`, `/api/pcs/{p}/config`, `/api/agents/{p}/effective_config`
- [x] CLI: `kanade config [get|set|unset|clear|effective]`
- [x] `agent.toml::[inventory]` を deprecate (sample ファイルからは削除済み、parser は互換維持)
- [x] `kanade-backend::main` が startup で JetStream resources を一括 auto-bootstrap (v0.3.1)

### Sprint 7+: 残バックログ

- [ ] 監視・メトリクス (Prometheus exporter)
- [ ] 大規模テスト (シミュレーション 3000 台)
- [ ] バックアップ / 復旧手順
- [ ] Web UI が CLI と feature parity (run / ping / kill / revoke / agent publish の HTTP + SPA 化)
- [ ] mTLS for NATS (現状未実装)
- [ ] NATS 3 ノードクラスタ
- [ ] Backend 冗長化 + LB
- [ ] SQLite → Postgres 移行 (必要時)

### Sprint 8 (v0.4.0): Client App + KLP (エンドユーザー向け)

- [ ] `crates/kanade-shared/src/ipc/` — KLP v1 の params / result / error 型定義 + ts-rs export
- [ ] Agent: KLP listener (Windows: Named Pipe / Linux: UDS `/run/kanade/agent.sock` / macOS: UDS `/var/run/kanade/agent.sock`) + OS peer 認証 (Windows token / Unix peercred) + subscription manager。Agent 側は 3 OS 実装済み、Client 側の Unix 対応は後続
- [ ] Agent: `klp::handlers` (state / notifications / jobs / support / maintenance)
- [ ] Manifest schema 拡張: `user_invokable` / `category` / `display_name` / `display_description` / `icon`
- [ ] Notification Manifest (`notifications/*.yaml`) + `NOTIFICATIONS` Stream + `notifications_read` KV bucket
- [ ] Backend HTTP API: `POST /api/notifications` (publish), `GET /api/notifications/{id}/ack_status` (確認状況)
- [ ] SPA に通知タブ追加 (作成・送信先指定・確認状況一覧)
- [ ] `crates/kanade-client` skeleton (Tauri 2.x)
- [ ] Client App: handshake / トレイ常駐 / 起動時未読通知ポップアップ / モーダル (emergency)
- [ ] Client App: 通知タブ / 状態タブ (コンプライアンスチェック) / アップデートタブ / トラブルシュートタブ
- [ ] Client App: サポート問い合わせ (`support.upload_diagnostics`) + メンテ予約延期 (`maintenance.defer`)
- [ ] MSI パッケージング (WiX) + ActiveSetup によるユーザー初回ログオン時の自動展開 (Windows)
- [ ] (後続段階) Linux / macOS の Client GUI・配置・自動起動・通知配送 (§2.11.7)
- [ ] (後続段階) Linux / macOS の Client の job ベース導入・更新・復旧 (既存 Kanade ジョブ + app-packages、macOS pkg / Ubuntu apt、§2.11.7.9)
- [ ] Client App 自己アップデート (`client_releases` Object Store bucket) (Windows のみ。Unix は §2.11.7.9 の job ベースで、この bucket を使わない)

### Sprint 8.5: Client App 拡張機能 (順次)

- [ ] パスワード期限通知 (AD 連携 or Windows API)
- [ ] VPN / プロキシのワンクリック再接続
- [ ] フィッシング報告ボタン (`events.security.report.{pc_id}`)
- [ ] セルフサービスソフトウェアカタログ (`category: catalog`)
- [ ] 言語切替 (ja/en) + ダーク/ライトテーマ

## 2.13 クロスプラットフォーム デプロイ設計 (kanade deploy CLI & Typed Operations)

本節は、Windows / Linux / macOS の全 operator 環境から利用可能な統一 CLI `kanade deploy` と、登録済み Agent 上で安全に動作する型付き操作（Typed Native Operations）の設計仕様である。現行の PowerShell / Bash による個別フリートデプロイスクリプト（`scripts/fleet-deploy.ps1`, `deploy/linux/fleet-deploy.sh` 等）を置き換え、プラットフォーム間の不整合や平文シークレットの漏洩リスク、バージョン/成果物のキー衝突を解消する。

本節の新 deploy CLI / typed operation / identity API は **設計が中心で、実装済みなのは Task 1–2、Task 3 Stage 3 の schema・検証・変換、Task 4 Stage 4 の Backend plan 永続化・認可 gate・capability preflight のみ**（§2.13.8.4）。typed の配送・実行・native handler・CLI は未実装で（plan が承認済みでも配送は常に拒否）、既存運用は移行 gate 完了まで継続する。以下の具体的 schema / option / timeout / policy は、§2.13.1.2 の確定製品判断を除き技術 proposal である。

### 2.13.1 背景・スコープ・確定判断と技術 proposal

#### 2.13.1.1 背景と現行 fleet-deploy スクリプトの課題
現行のデプロイ運用は、Windows 向けの `scripts/fleet-deploy.ps1` と Linux 向けの `deploy/linux/fleet-deploy.sh` という二つの独立したスクリプトに分断されている。
- **プラットフォーム間の実装乖離**: Windows 版の publish / job / exec / agent rollout は現在 CLI の HTTP API 経由（Backend から Agent への配送は NATS）であるのに対し、Linux 版は SSH とオフライン tarball 配布（`--mode install`）または既存 HTTP publish / rollout による Agent 自己更新（`--mode update`）という全く異なるモデルで動作している。
- **平文シークレットの危険な露出**: `fleet-deploy.ps1` は一時スクリプト生成時に JWT シークレットや管理者パスワード、NATS トークンを平文でパラメータ注入しており、プロセス引数や一時ファイル、ジョブ履歴への漏洩リスクを抱えている。
- **成果物キーの衝突**: `kanade app publish` の Object Store キーが `<name>/<version>` のみで定義されているため、同一バージョンにおける Windows PE、macOS `.app` / `.pkg`、Linux パッケージが同一キーで上書き衝突する。
- **PE 前提のバージョン検出**: バージョン自動検出が Windows PE の `VERSIONINFO` (`pelite`) に依存しており、Unix バイナリや非 PE 成果物で破綻している。
- **未対応ホストでのサイレント障害**: フリート指定（`--all` / `--groups`）時に、未対応の OS やアーキテクチャ、成果物不足の端末が存在しても事前に検出されず、一部端末でデプロイが失敗または無応答となる。

#### 2.13.1.2 確定した製品判断 (Confirmed Operator Decisions)
運用者が承認済みの製品判断（**確定事項**）は以下の通りである。

1. **統一オペレータ入口**: 最終的にサポートされる運用者の唯一のデプロイ入口は、全 operator OS（Windows, macOS, Linux）共通の `kanade deploy` CLI である。並行する PowerShell / Bash スクリプトや永続的なラッパーは廃止する。
2. **対象スコープの限定**: デプロイ対象は「既存の登録済み Agent（enrolled agents only）」に限定する。
3. **初回収束の境界維持**: 初期 Agent bootstrap は**明示的にスコープ外（OUT OF SCOPE）**である。Web UI の Agent Installation 画面で提供されるインストールワンライナー（`installer.ps1`, `installer.sh`）は一切変更せず維持する。CLI に SSH bootstrap 機能を組み込まない。
4. **コンポーネント別更新契約**:
   - **Client / Backend / Admin CLI**: 既存の Kanade jobs、app-packages 配布機構、および実行監査経路を使用する。独立した自律型アップデータや専用の `client_releases` サービスは設けない。
   - **Agent**: 既存の agent publish / rollout セマンティクス（NATS `agent_releases` Object Store、`agent_config` の `target_version` / `target_version_jitter`、および SCM / systemd 再起動）を維持する。
5. **Typed Native Operations (型付きネイティブ操作)**:
   - OS 固有の特権インストールロジックは、単なる `.ps1` や `.sh` スクリプトの隠蔽コピーではなく、固定・検証された引数でネイティブプラットフォームツール（Windows SCM, macOS installer, Ubuntu apt/dpkg）を直接呼び出す型付き Rust agent operations とする。
   - これにより、汎用シェルジョブや無関係なビルド/bootstrap スクリプトは削除されず存続する。
6. **秘密情報の排除原則**: オペレータの秘密情報（JWT secret, NATS token/password, DB password, admin password 等）をジョブ本文、引数、計画、ログ、公開成果物へ載せない。ターゲットホスト上の既存設定・セキュアストレージ値の保持を既定とし、未整備の秘密配送が必要な操作は実行を拒否する。
7. **ハッシュと署名の信頼性**: パッケージのハッシュ単体を publisher 認証の代用としない。macOS の ad-hoc 署名も配布信頼の代用としない。
8. **Client App スコープの遵守**:
   - macOS: macOS 14 以降、Apple Silicon (aarch64) の管理/試験用 ad-hoc 署名（信頼された公証 notarized 配布ではない）。
   - Linux: Ubuntu 24.04 LTS x86_64 GNOME (Wayland / X11) を初期ターゲットとする。
   - Windows: 既存の回帰テスト・動作を維持。
   - GUI セッション境界の安全性、ユーザー opt-out の尊重、ジョブ駆動の install/update を維持する。
   - 不足している client packaging を明確に特定し、延期された GUI 機能を実装済みと偽らない。
9. **スクリプトの段階的削除**: 既存スクリプト（`scripts/fleet-deploy.ps1`, `deploy/linux/fleet-deploy.sh` 等）は移行期間中機能維持し、新 CLI / Agent 契約との同等性（parity）が実機検証され、文書が置き換えられた後にのみ後続タスクで削除する。本変更（docs タスク）ではコード実装やスクリプト削除を行わない。

#### 2.13.1.3 技術 proposal (Technical Proposals & Recommendations)
本設計で提案・推奨する技術仕様は以下の通りである。

- **CLI 体系**: `kanade deploy plan <role>` / `apply <plan-id>` / `status <exec-id|plan-id>` の 3 段階ワークフローを推奨する。
- **成果物 Identity とキー体系**: `(role, version, os, arch, distro_abi, package_format)` の多元タプルによる厳密な識別、および署名付きマニフェスト (`package.manifest.json`) の導入。
- **旧 Bare Key の移行**: Windows 向けレガシーキー（`<version>` や `<name>/<version>`）は、PE ヘッダ検査で Windows 成果物と確認できたもののみ明示的にエイリアス移行する。
- **二重防御による下位互換性**: 旧 Agent に対する Capability Preflight（事前照合）と、Command wire レベルでの必須 Discriminator による安全な拒否（Fail-closed）。
- **Backend 制御プレーン置換**: Agent 側の Outbox 耐久性と結果再投影の検証完了をもって成功とする契約。`-WipeDb` の自動バックアップと明示的確認。

#### 2.13.1.4 スコープ境界の厳格な定義 (Out-of-Scope vs In-Scope)

| 区分 | 対象項目 | 理由・方針 |
|---|---|---|
| **In-Scope (置換対象)** | 登録済み Agent に対する backend / client / cli のインストール・更新 | 統一 CLI `kanade deploy` と Typed Native Operations ジョブへ移行 |
| **In-Scope (維持・統合)** | 登録済み Agent 自身の self-update | 既存の `agent publish` / `rollout` セマンティクスを `kanade deploy agent` に統合 |
| **Out-of-Scope (存続)** | Web UI の Agent Installation 画面ワンライナー (`installer.ps1`, `installer.sh`) | 未登録ホストの初回収束用として不変のまま維持 |
| **Out-of-Scope (存続)** | `deploy/linux/setup.sh`, `setup-agent.sh`, `scripts/deploy/nats.ps1` | 新規サーバー・ブローカーのベアメタル/VM 初期構築スクリプトとして存続 |
| **Out-of-Scope (非採用)** | CLI への SSH bootstrap 機能追加 | Agent 未導入ホストへの SSH 操作は kanade CLI の責務外 |
| **Out-of-Scope (存続)** | 汎用 shell ジョブ (`sh`, `powershell`, `pwsh`, `cmd`) | 一般運用タスク用として変更なし |

---

### 2.13.2 既存デプロイ手段の棚卸しと移行マッピング

#### 2.13.2.1 既存スクリプト・オプション・挙動の包括的インベントリ
既存のデプロイスクリプトが受け取る全パラメータおよびその実動作を以下に整理する。

- **`scripts/fleet-deploy.ps1` (Windows)**:
  - `-Role`: `backend` | `agent` | `client` | `cli`。backend/client/cli はジョブ実行、agent は publish + rollout。
  - `-Pc`, `-Groups`, `-All`: ターゲット指定。`-Pc` はケースセンシティブで NATS subject に直結。`-All` は cli ロールで拒否。
  - `-Jitter`: agent rollout 時のダウンロードばらけ時間。
  - `-Version`: `latest` (GitHub Release 問い合わせ), 空白 (PE から抽出), SemVer。
  - `-ExePath`, `-GitHubRepo`: ローカル成果物指定と Release 解決先（既定 `kanadehq/kanade`）。
  - `-Stage`: `build-release.ps1` を呼んでローカルにバイナリを事前ステージ。
  - `-Server`, `-BackendUrl`, `-SourceUrl`, `-AuthToken`, `-NatsToken`: 通信・認証パラメータ。
  - `-WipeDb`: backend の SQLite DB wipe フラグ。
  - `-JwtSecret`, `-StaticToken`, `-BootstrapAdminPassword`, `-MailPassword`: 一時スクリプト・マニフェストへの秘密情報注入。
  - `-NoVerify`: デプロイ後のバージョン検証スキップ。
  - `-DryRun`: 実行コマンドのプレビュー（一時ファイルは生成・保持）。
- **`deploy/linux/fleet-deploy.sh` (Linux)**:
  - `--role`: `backend` | `agent`。
  - `--mode`: `install` (SSH 経由で bundle コピー & `setup.sh` 実行) | `update` (HTTP API 経由で agent publish + rollout)。
  - `--host`, `--identity`, `--domain`: SSH 接続先と Caddy/NATS 用ドメイン。
  - `--arch`: `x86_64` | `aarch64`。
  - `--pc`, `--group`, `--all`, `--jitter`: update モードのスコープ。
  - `--nats-url`, `--nats-token`: NATS 接続設定。
  - `--version`: update は明示版、未指定は workspace 版。install は bundle の版指定。
  - `--agent-bin`, `--backend-bin`: ローカル成果物。`--agent-bin` は enrolled-agent update でも使用。
  - `--github-repo`: Release ダウンロード元。`--out`: install 用 bundle 出力先。

#### 2.13.2.2 3分類移行対応表
上記インベントリの全入力・挙動を「型付きジョブへ移行」「bootstrap 手順として存続」「危険な既定値や設定注入として廃止」に分類する。

| スクリプト | オプション / 動作 | 分類 | 移行先 / 処置理由 |
|---|---|---|---|
| `fleet-deploy.ps1` | `-Role backend/client/cli` | **型付きジョブへ移行** | `kanade deploy plan <role>` + Typed Native Operation |
| `fleet-deploy.ps1` | `-Role agent` | **型付きジョブへ移行** | `kanade deploy plan agent` (内部で agent publish/rollout を実行) |
| `fleet-deploy.ps1` | `-Pc <id>` | **型付きジョブへ移行** | `--pc <id>` (登録名の大文字小文字を完全一致で維持) |
| `fleet-deploy.ps1` | `-Groups <list>` / `-All` | **型付きジョブへ移行** | `--group <name>` / `--all` (Fail-closed による事前検証付き) |
| `fleet-deploy.ps1` | `-Jitter <dur>` | **型付きジョブへ移行** | `--jitter <dur>` (Agent rollout およびジョブ配信の分散) |
| `fleet-deploy.ps1` | `-Version latest` | **型付きジョブへ移行** | 不変バージョン解決 (`plan` 時に版・digest を固定) |
| `fleet-deploy.ps1` | `-Stage` | **型付きジョブへ移行** | CLI に統合せず、ローカル成果物の指定 (`--binary <path>`) で代替 |
| `fleet-deploy.ps1` | `-SourceUrl` | **型付きジョブへ移行** | 後掲の `--source-url` / endpoint policy に移行。署名 manifest の identity / digest と到達可能な download endpoint を別々に検証し plan に固定 |
| `fleet-deploy.ps1` | `-DryRun` | **型付きジョブへ移行** | `kanade deploy plan` (副作用なしの完全 Read-Only 実行) |
| `fleet-deploy.ps1` | `-JwtSecret`, `-StaticToken`, `-BootstrapAdminPassword`, `-MailPassword` | **危険な既定値・注入として廃止** | ジョブ本文・引数への平文シークレット注入を全廃。ターゲット上の既存設定（HKLM / env）保持を既定化 |
| `fleet-deploy.ps1` | `-AuthToken`, `-NatsToken` (平文引数) | **危険な既定値・注入として廃止** | オペレータ環境の `KANADE_AUTH_TOKEN` / NATS 資格情報を使用し、ターゲットには配送しない |
| `fleet-deploy.ps1` | `-WipeDb` (簡易指定) | **危険な既定値・注入として廃止** | 暗黙実行を禁止。明示的 `--wipe-db` + 自動バックアップ + 二重確認に移行 |
| `fleet-deploy.ps1` | `-NoVerify` | **危険な既定値・注入として廃止** | 完全廃止。常に配置検証とロール・起動方針に応じた稼働検証を実施 |
| `fleet-deploy.sh` | `--mode update` (agent) | **型付きジョブへ移行** | `kanade deploy plan agent` に統一 |
| `fleet-deploy.sh` | `--mode install` (SSH bootstrap) | **bootstrap 手順として存続** | CLI スコープ外。初期ホスト構築スクリプトとしてそのまま存続 |
| `setup.sh`, `setup-agent.sh` | 全機能 (ベアメタル構築) | **bootstrap 手順として存続** | CLI スコープ外。初回収束・オフライン構築用として存続 |
| `scripts/deploy/nats.ps1` | 全機能 (ブローカー構築) | **bootstrap 手順として存続** | CLI スコープ外。ブローカー初期配備用として存続 |
| Web UI ワンライナー | `installer.ps1`, `installer.sh` | **bootstrap 手順として存続** | CLI スコープ外。初期エージェント登録用として不変維持 |

追加の入力・installer 挙動も削除前の parity 対象とする。以下の新オプションは technical proposal（推奨）であり、既存設定を黙って変更しない。

| 現行入力 / 挙動 | 移行契約 |
|---|---|
| Windows `-ExePath`、Linux `--agent-bin` / `--backend-bin` | `plan --binary <path> --version <ver>`。ローカル読取のみ、identity・実版・publisher 検証必須。apply 時に app publish / agent publish で格納。PE のない成果物は版省略不可 |
| Windows `-GitHubRepo`、Linux `--github-repo` / `--version` | `--github-repo <owner/repo>` / `--version <ver>`。別 repo も許すが信頼済み publisher は変更しない。未署名ローカル build は信頼済み署名を準備するまで拒否 |
| Linux `--out` | bootstrap bundle 出力として存続。deploy は bundle を生成しない |
| Linux `--host` / `--identity` / `--domain`、install の `--arch` | bootstrap のみ。fleet wrapper 削除時も setup/build 手順で代替し SSH を CLI に移植しない。update の arch は inventory と照合する `--arch` assertion へ |
| Windows `-Server` / `-BackendUrl` / `-SourceUrl` | operator 接続設定と target download endpoint を分離。`--server` / `--backend-url` / `--source-url`（許可済み HTTPS endpoint のみ）。loopback 推測を廃止、到達性検査後 plan に固定 |
| backend.ps1 `SourceDir` / `ServiceName` | artifact identity / 管理所有権 receipt で代替。任意サービス名・任意パスを停止しない |
| `ForceConfig` / `FreshInstall` | config 強制上書き・秘密情報初期化は deploy から廃止。enrolled host 上の初回 backend install は検証済み既存 config 必須、不在なら設定準備を要求して拒否 |
| `FirewallPort` / `NoFirewall` | 明示 `--firewall-port <1..65535>` / `--no-firewall`（相互排他）を plan に固定。既定は既存 firewall 保持、public bind から自動開放しない。固定名の Kanade 所有 rule のみ変更 |
| `Recreate` | `--repair-service`。所有権・実行パス照合後、既存 service 設定を保存し管理 service のみ再登録。承認対象、失敗時は設定復元 |
| `NoStart` | `--start-policy preserve|never`。既定 preserve は停止済み service を起動しない。never は配置検証だけで成功 |
| backend.ps1 `NatsToken` / `NatsUser` / `NatsPassword` と他 secret | deploy 引数として廃止。既存設定保持、初回設定は別の安全な provisioning 手順。値を plan や job に記録しない |
| Windows `KANADE_TARGET_PC` fallback、複数 Groups、agent の単一 group 制約 | 解決済み対象集合を plan に表示・固定。CLI は反復 `--group` を job roles に許可、agent は既存 rollout の単一 scope 制約を維持 |
| 既存 jitter 省略時の scope 値維持、NoVerify、DryRun の一時ファイル | agent は省略時に既存 scope jitter を維持し、値を plan に表示。新 job jitter の既定は §2.13.5.4。NoVerify と secret 入り一時 script 生成は廃止 |

#### 2.13.2.3 秘密情報・トークン・危険なフラグの安全化方針
1. **秘密情報の不可侵化**:
   - デプロイジョブはターゲットマシン上の既存秘密情報（Windows レジストリ `HKLM\SOFTWARE\kanade\backend`、Linux `/etc/kanade/kanade.env`）を一切上書きせず、完全に保持する。
   - 新規シークレットのプロビジョニングが必要な場合は、デプロイ CLI の引数やジョブ定義に平文で含めることを禁止し、暗号化された秘密管理チャネル（将来の Vault 連携または専用の KLP チャネル）を必須とする。未整備の環境では設定変更操作を拒否する。
2. **`-WipeDb` の安全化**:
   - デフォルトは常にデータベース保護（No Wipe）。
   - 通常更新と `--wipe-db` の両方で、§2.13.7.5 の整合 snapshot を必須とする。DB / WAL の稼働中単純コピーはバックアップとみなさない。
   - `users` / `permission_groups` / `user_passkeys` を保護する。その他の非再生成データと replay source / retention の調査も wipe 承認の前提とし、再投影 readiness と結果投影まで検証する (§2.13.7.4–5)。
3. **`-NoVerify` の廃止と検証の厳格化**:
   - ジョブがキューに受理されたこと（Admission）と、実際にサービスが起動して正しいバージョンで応答したこと（Verified Success）を明確に区別する。
   - タイムアウト付きでヘルスチェックと実版照合を自動実行し、未達の場合は失敗（または警告付き延期）として報告する。

---

### 2.13.3 公開 Release 成果物の実態と packaging 課題

#### 2.13.3.1 リポジトリワークフロー・ビルドスクリプト調査結果
`.github/workflows/release.yml`, `release-extras.yml`, `client-macos.yml` および各種ビルドスクリプトの実態調査結果は以下の通りである。

- **`release.yml` (kata 管理)**:
  - ワークスペース内の全 `[[bin]]` ターゲット（`kanade`, `kanade-agent`, `kanade-backend`, `kanade-client`）をビルド。
  - 成果物アーカイブ形式（workflow の `stem="$bin-$TARGET"` で内部 executable にも target suffix が付く。handler はこれを固定 role 名へ正規化する）:
    - Windows (`x86_64-pc-windows-msvc`): `<bin>-x86_64-pc-windows-msvc.zip` (内部に `<bin>-x86_64-pc-windows-msvc.exe`)
    - Linux x86_64 (`x86_64-unknown-linux-musl`): `<bin>-x86_64-unknown-linux-musl.tar.gz` (内部に `<bin>-x86_64-unknown-linux-musl`)
    - Linux aarch64 (`aarch64-unknown-linux-musl`): `<bin>-aarch64-unknown-linux-musl.tar.gz` (内部に `<bin>-aarch64-unknown-linux-musl`)
    - macOS aarch64 (`aarch64-apple-darwin`): `<bin>-aarch64-apple-darwin.tar.gz` (内部に `<bin>-aarch64-apple-darwin`)
  - 注意: Intel Mac (`x86_64-apple-darwin`) はビルドマトリクスから明示的に除外されている。
- **`release-extras.yml`**:
  - `publish-client-to-backend`: Windows 向け `kanade-client.exe` を backend の `/api/app-packages/kanade-client/<version>` にアップロード。
  - `sbom`: 各バイナリの CycloneDX SBOM を生成して GitHub Release に添付。
- **`client-macos.yml` / `scripts/build-client-macos.sh`**:
  - Apple Silicon Mac 上で Tauri アプリケーションをビルドし、`Kanade Client.app` バンドルを作成。
  - `/usr/bin/codesign --force --sign -` による ad-hoc 署名を実施。

#### 2.13.3.2 成果物マトリクス (実物アセットと配布形式)

| ロール | プラットフォーム | release.yml が生成する想定名 (実物未検証) | 実行・配置形式 |
|---|---|---|---|
| **agent** | Windows x86_64 | `kanade-agent-x86_64-pc-windows-msvc.zip` | `kanade-agent.exe` (Windows サービス) |
| **agent** | Linux x86_64 / aarch64 | `kanade-agent-<arch>-unknown-linux-musl.tar.gz` | `kanade-agent` (systemd ユニット) |
| **agent** | macOS aarch64 | `kanade-agent-aarch64-apple-darwin.tar.gz` | `kanade-agent` (LaunchDaemon) |
| **backend** | Linux x86_64 / aarch64 | `kanade-backend-<arch>-unknown-linux-musl.tar.gz` | `kanade-backend` (systemd ユニット) |
| **backend** | Windows x86_64 | `kanade-backend-x86_64-pc-windows-msvc.zip` | `kanade-backend.exe` (Windows サービス) |
| **cli** | Windows / Linux / macOS | `kanade-<target>.zip` / `.tar.gz` | `kanade` (単一バイナリ) |
| **client** | Windows x86_64 | `kanade-client-x86_64-pc-windows-msvc.zip` | `kanade-client.exe` (Tauri GUI) |
| **client** | macOS aarch64 | `kanade-client-aarch64-apple-darwin.tar.gz` (単体 bin のみ) | **未整備** (要 `Kanade Client.app` / `.pkg`) |
| **client** | Linux x86_64 | `kanade-client-x86_64-unknown-linux-musl.tar.gz` (単体 bin のみ) | **未整備** (要 Ubuntu 24.04 `.deb`) |

#### 2.13.3.3 Client Packaging ギャップと未整備課題の特定
Release ワークフローの記述だけで「全環境で即座に配布可能」と断定することはできない。以下の深刻な packaging ギャップが存在する。
1. **macOS Client のインストーラ未整備**:
   - `release.yml` が生成するのは単体の Mach-O バイナリ (`kanade-client`) のみである。
   - macOS 上で GUI アプリケーションとして動作し、通知やトレイ常駐を行うには `Contents/Info.plist`, `Resources/icon.icns` を含む `Kanade Client.app` バンドルが必須である。
   - さらに、Kanade ジョブから非対話的に特権インストールを行うには、`/Applications` への配置と LaunchAgent 登録をパッケージ化した `.pkg` インストーラ形式が必要であるが、CI パイプラインには `.pkg` 生成ステップが存在しない。
2. **Ubuntu Linux Client の GUI とパッケージの未整備**:
   - 現行 `crates/kanade-client/src/main.rs` の Linux entrypoint はメッセージを出して exit(1) する shim であり、GUI は未実装。musl tarball を .deb に包んでも GUI Client にはならない。
   - 最初に Ubuntu 24.04 x86_64 GNOME 向け Tauri / WebKitGTK の GUI entrypoint と対応ビルド（GNU target と依存ライブラリの検証）を整備する。§2.11.7 の session / KLP / 通知等の未実装契約は独立した前提条件であり、この設計で実装済みとはしない。
   - Ubuntu 24.04 (GNOME) でデスクトップ統合（`.desktop` ファイル、アイコン、WebKitGTK 依存関係、ログイン起動ユニット）を行い、`apt` / `dpkg` で整合性を保って管理するための `.deb` パッケージ生成パイプラインが存在しない。
3. **Packaging 整備の独立性**:
   - macOS GUI のコードベース実装（Tauri）が存在することと、配布可能な `.pkg` / `.deb` パッケージが存在することは別問題である。
   - これらのパッケージングパイプラインの構築は、デプロイ CLI の本格稼働に向けた必須前提タスクとして独立して計画・管理されなければならない。

#### 2.13.3.4 配布信頼の現実と延期事項の保持
1. **macOS ad-hoc 署名の現実**:
   - 初期の管理/試験用 Mac 配備は ad-hoc 署名（`-s -`）であり、Apple Developer ID 署名および公証（Notarization）は**延期**されている。
   - ad-hoc 署名は Gatekeeper の信頼を確立しないため、初回起動時の手動許可（System Settings → Privacy & Security）または MDM プロファイルによるバイパスが前提となる。パッケージの SHA-256 ハッシュや ad-hoc 署名を「公証配布と同等の信頼」と偽ってはならない。
2. **延期事項の厳格な維持**:
   - 以下の環境・機能は引き続き**延期（Deferred）**であり、本デプロイ設計でも「対応済み」と偽らず、明示的にスコープ外（非対応）として扱う。
     - Linux **Client のみ**: ARM (aarch64), Debian, Fedora (`dnf`), KDE, その他ディストリビューション。非 Client の既存 Linux ARM agent publish/rollout と release 成果物はこの制限に含めない
     - macOS: Intel (`x86_64`), Apple Developer ID 署名, Notarization
     - Client: 独立した自動アップデータ、`client_releases` 専用バケット、外部 package 管理の黙認引き継ぎ

---

### 2.13.4 成果物 Identity・不変性・Cross-OS 衝突防止

#### 2.13.4.1 多元タプルによる Artifact Identity 定義
クロス OS 環境でのパッケージ衝突を完全に防ぐため、成果物の一意性を以下の多元タプルで厳格に定義する。

**Stage 1a 実装済み**: `kanade_shared::artifact` が identity / integrity の唯一の共有契約。`ArtifactIdentity` は非公開 fields と検証付き constructor / Deserialize を持ち、JSON fields は `role`, `version`, `os`, `arch`, `distro_abi`, `format` の順に serialize する。未知 field / enum、欠落 / null ABI、未許可の組合せは拒否する。operator OS を target の推測に使用しない。
SemVer 構文検証は `semver` クレートを使用する。`ArtifactError` は identity と key の専用エラーを区別し、Linux aarch64 Client は `ClientLinuxAarch64Unsupported` として拒否する。

| role | os / arch | 必須 distro_abi | format |
|---|---|---|---|
| backend / client / cli / agent | windows / x86_64 | msvc | exe / zip |
| backend / cli / agent | linux / x86_64 または aarch64 | musl | tar_gz |
| client | linux / x86_64 | ubuntu-24.04 | deb |
| client | macos / aarch64 | darwin14 | pkg |
| cli / agent | macos / aarch64 | darwin | tar_gz |

これは **identity を表現できる範囲** であり、release に package が存在すること、内容の検査、署名信頼、実機対応、deployability の保証ではない。Client の GUI / session 要件は §2.11.7 を維持する。macOS Backend、Intel Mac、Windows ARM、Linux ARM Client、その他 ABI / format の新規対応は含めない。非 Client macOS の `darwin` は最低 OS 版を主張しない。Agent identity の archive 表現も既存 release / rollout 契約を変更しない。

版は `ArtifactVersion` の canonical SemVer 2.0 文字列（最大128 ASCII bytes、core 3整数は u64、先頭ゼロ不可）。prerelease / build metadata は保持し、大小文字も区別する。operator 入力用 `from_input` だけ既存 Agent の `normalize_version`（前後空白と先頭 `v` 1個の除去）を再利用する。保存 JSON / key ではこの補正をしない。package revision / 再build と SemVer build metadata の業務上の対応は未決定であり、metadata を版更新や不変性検証の代用にしない。Windows の exe / zip は異なる identity として表現するが、今後の deploy がどちらを配布形式に選ぶかは未決定。

#### 2.13.4.2 Object Store キー設計と Windows Bare Key 互換移行
1. **新キー体系 (Stage 1a 共有契約、Stage 1b 保存・API 実装済み)**:
   - bucket 名と object key は別の値である。`OBJECT_APP_PACKAGES` の bucket は `app_packages`、その内部の canonical key は `<role>/<version>/<os>-<arch>-<distro_abi>.<format>`。role は `backend|client|cli` とし、bucket 名を key に重ねない。
     例: bucket=`app_packages`, key=`client/0.80.0/macos-aarch64-darwin14.pkg`。
     例: key=`client/0.80.0/linux-x86_64-ubuntu-24.04.deb`、`client/0.80.0/windows-x86_64-msvc.exe`。
   - 唯一の文法は `<role>/<version>/<os>-<arch>-<distro_abi>.<format>`。各 token は上表の lowercase enum 値そのもの、ABI は省略不可。`tar_gz` は拡張子も **`.tar_gz`** とし `.tar.gz` は alias にしない。role は `backend|client|cli`、version には上記 canonical SemVer をそのまま使う。3 segment 以外、末尾 slash、case folding、URL decode、path normalization、空白補正は認めない。末尾の format を最後の `.` で分離し、OS / arch を先頭2個の `-` で取り、残り全体を ABI と照合するため `ubuntu-24.04` も一意。検証後の再生成が入力 key と byte 単位で一致することを要求する。bucket 名は key に含めない。許可表全体の往復・非衝突を試験する。
   - canonical JSON はこの型の固定 field 順による compact serialization を指す。署名用 manifest 全体の canonicalization や任意 JSON の canonicalization を定義・実装したものではない。
   - 現行 `api/app_packages.rs` は slash-free な `name` / `version` の2 segment API で、list もその形を仮定する。この新 key は現行 API にそのまま渡せない。§2.13.4.4 の別 identity API / namespace の設計と producer / list / download / delete / CLI 全 consumer の対応が前提。Stage 1b は同じ bucket の canonical key と別 `/api/app-artifacts` API を実装。既存 CLI / producer は移行しない。契約・権限前提・retry / offline cleanup は [app-artifacts.md](app-artifacts.md) を参照。
   - `OBJECT_AGENT_RELEASES` の bucket は `agent_releases`。既存の `AgentPlatform::release_key` を維持する。Windows: `<version>`、Linux: `<version>-linux-<arch>`、macOS: `<version>-macos-aarch64`。app identity key をこの bucket へ適用しない。
2. **レガシー Windows Bare Key の安全な移行（移行自体は未実装）**:
   - Stage 1a の `inspect_legacy_windows(role, declared_version, bytes)` は旧 key 名を入力にせず、既存 binary platform parser と object parser で PE/COFF AMD64 executable（DLL / object 不可）を確認し、利用可能な VERSIONINFO と明示版の不一致を拒否する。結果は未認証 `LegacyWindowsCandidate` に限定する。VERSIONINFO 不在は `embedded_version_verified=false` として残す。role は caller の宣言であり PE header から証明しない。SHA-256 / size が一致しても publisher provenance の証明ではない。alias 作成・snapshot・配布許可は行わない。
   - 既存の `OBJECT_APP_PACKAGES` に存在する `<name>/<version>` 形式のオブジェクトは、一律に Windows 用とみなすのではなく、バイナリの `MZ` / PE header と Machine=`AMD64` (`0x8664`)、実版・role・publisher provenance を検査し、**Windows x86_64 成果物であると明示的に確認されたもののみ**、新キー体系の Windows エイリアスとして解決する。
   - PE ヘッダを持たない非 Windows 成果物がレガシーキーに存在する場合はエラーとし、推測による上書きや他 OS への配信を行わない。

#### 2.13.4.3 署名付きマニフェスト (`package.manifest.json`)

stage 2b の publisher trust 永続化は Unix のみ対応する。Windows を含む非 Unix は
owner／DACL／祖先の保護・プロセス間排他・耐久性のある原子的置換の実装待ちであり、
root provisioning は保存先作成前に拒否し、既存 trust の読込・state 更新・新規 package
admission も fail closed とする。手動 ACL 設定や管理操作の直列化では有効化できない。
未設定の Agent の既存処理は継続する。詳細と管理手順は `docs/package-manifest.md`。

成果物の配布時には、バイナリ本体と対になる署名付きメタデータマニフェストを Object Store に併せて格納する。

```json
{
  "manifest_version": "1.0",
  "identity": {
    "role": "client",
    "version": "0.80.0",
    "os": "macos",
    "arch": "aarch64",
    "distro_abi": "darwin14",
    "format": "pkg"
  },
  "package_id": "com.kanade.client",
  "min_os_version": "14.0",
  "required_agent_capabilities": ["typed_deploy_v1", "macos_pkg_installer_v1"],
  "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "size_bytes": 12451840,
  "publisher": {
    "key_id": "ed25519:kanade-rel-2026",
    "signature": "base64-encoded-signature"
  }
}
```

- Stage 1a は `ArtifactIntegrity { sha256, size_bytes }` と検証付き `ArtifactSha256` を実装。JSON の SHA-256 は64文字 lowercase hex、Object Store digest は `SHA-256=` + 32 bytes の padded URL-safe base64 と明示変換する。既存 CLI の read-back digest 整形のみ共通化する。署名付き manifest の **オフライン契約は Stage 2a 実装済み**（下記）。publisher registry・管理された信頼配布は未実装。
- 使用例・鍵管理の責任・正規形・終了コード・検証記録は [署名 manifest 運用](package-manifest.md) を参照。
- **Stage 2a 実装済み（オフライン署名契約のみ）**: `kanade_shared::package_manifest` と `kanade app sign-artifact` / `verify-artifact` / `publisher-trust`。
  - **スキーマ v1**: 上例に加え `publisher.algorithm`（`"ed25519"` のみ）を必須とする。`manifest_version` は `"1.0"` のみ受理し、それ以外は専用エラー。`identity` は既存 `ArtifactIdentity` をそのまま検証し、`sha256` / `size_bytes`（0 不可）は `ArtifactIntegrity` と同一表現。全 field 必須、未知・重複 field、欠落、末尾データを拒否する。`min_os_version` は **非 null の必須文字列**（1–4 個のドット区切り十進数、各 1–9 桁。空文字不可）。`package_id` は `[a-z0-9._-]` 128 byte 以下、`required_agent_capabilities` は `[a-z0-9_]` 64 byte 以下・最大 32 件・厳密昇順（ソート済み・重複なし）。
  - **署名 bytes**: `kanade/package-manifest/ed25519/v1\0` に続けて、`publisher.signature` を除いた manifest の固定 field 順 compact UTF-8 JSON（`publisher.algorithm` と `publisher.key_id` を含む）。ファイル形式は同じ compact JSON + 末尾 `\n` 1 つ（省略可）で、空白・順序・escape の別表記は署名が有効でも `NonCanonical` として拒否する。表現を変えるには `manifest_version` の更新が必要。署名は標準 padded base64 の 64 byte で、非正規 encoding・長さ違いを拒否し、検証は `verify_strict`。
  - **鍵と権限の分離**: publisher 鍵は command 署名鍵（`signing`）と別権限。`Signer` / `KeyRing` は使わず、domain prefix で署名 bytes を分離する。ただし同一秘密鍵を両用途に使い回すこと自体は暗号的に検出できないため、用途別の鍵を運用で分けること。`key_id` は `ed25519:` + 公開鍵 32 byte に domain prefix を付けた SHA-256 全体の lowercase hex（上例の `ed25519:kanade-rel-2026` は例示で、v1 の `key_id` ではない）。
  - **信頼入力**: 検証は manifest / package に埋め込まれた鍵を使わず、独立に取得した trust record（`purpose = kanade-package-publisher-key-v1`、`algorithm`、`key_id`、base64 公開鍵。厳密 parse で弱い公開鍵を拒否し `key_id` を再計算照合）のみを信頼する。検証者は期待する完全な `ArtifactIdentity` も別入力として渡し、正しく署名された別 role / version / platform の manifest への差替えを拒否する。
  - **CLI**: すべてローカル完結（backend / NATS / Object Store 不使用、自動更新確認も抑止）。秘密鍵（32 byte seed の padded base64）は非表示 prompt か明示的な `--secret-stdin` のみ。argv・環境変数は不可、manifest・trust record・出力・エラーに出さない。ファイルから stdin に流す場合はその鍵ファイルを所有者のみ（mode 0600 / owner-only ACL）にし、共有・同期領域に置かない。`sign-artifact --out` は既存ファイルを上書きしない。`--min-os-version <v>` が必須。終了コード: 成功 0、操作・検証失敗 1、clap 引数不備 2。
  - **意味の限界**: 成功出力は `OFFLINE VERIFIED (signature + integrity only)` であり、現在の publisher 認可・鮮度・失効は **未確認**。`OfflineVerified` に deploy-ready / privileged install への変換は無く、このコマンドの結果だけで配備や特権導入を許可してはならない。managed trust・rotation・失効・freshness と Agent 側 admission 検証は下の Stage 2b。
  - **Stage 2b: 管理された publisher 信頼と admission 検証（実装済み・配送は blocked）**: 詳細は [package-manifest.md](package-manifest.md)。
    - **信頼の起点**: 管理者が明示的に pin する専用 trust root 公開鍵（`purpose = kanade-package-trust-root-v1`、`trust-store-provision`）のみ。NATS・store・package・manifest から first-use で学習せず、`apply` は root を変更せず、異なる root での再 provision は拒否する。root 鍵は publisher 鍵・command 署名鍵と別権限で、署名 domain も別（`kanade/package-trust-state/ed25519/v1\0`）。state が root 鍵を publisher として列挙すれば拒否する。
    - **trust state v1**: `trust_set_id`、単調増加 `sequence`、`issued_at`/`not_after`（unix 秒、有効期間は最大 90 日）、期間付き active publisher、累積 revoked key id。固定順 compact JSON を root が署名（`TrustedPublisherKey` と `verify_strict` を再利用）。未知／重複 field、重複鍵、active と revoked の衝突、不正期間は拒否。同一 sequence の同一内容のみ冪等、低 sequence は rollback、同 sequence 別内容と失効の取消しは conflict。rotation は active 期間を重ねた state の後に旧鍵を恒久失効させる。
    - **永続化**: 署名検証と後継規則の確認後にのみ、private な一時ファイル→fsync→rename→dir fsync で `state.json`（署名済み state と時刻 high-water を 1 レコード）を置換する。毎回再読込・再検証し、破損・改ざん・権限過大・symlink は `Broken` で fail closed（初期状態へ自動復旧しない）。
    - **admission**: `package_admission::admit_package` は外部指定の完全な `ArtifactIdentity`・digest/size・package_id・互換性 policy（platform、数値の OS 下限、capability）と、現在有効な publisher 認可（鮮度・active・非失効・期間内・時計確実）をすべて要求する。結果 `AdmittedPackage` は identity・digest・trust sequence に束縛された証拠で、`OfflineVerified` からは作れず、install 権限ではない。trust 未設定の Agent でも既存動作は不変で、新規 package admission のみが拒否される。
    - **時計と再起動**: wall clock が永続 high-water より 5 分超遅い、monotonic との乖離が 5 分超、high-water を保存できない場合は admission を拒否。state が `not_after` を過ぎても同様。管理者によるディスク全体の巻戻しは防げない。
    - **管理 CLI**（すべてローカル、秘密は非表示 prompt／`--secret-stdin`）: `trust-root-record`、`trust-state-sign`、`trust-store-provision`、`trust-store-apply`、`trust-store-status`（公開識別子・時刻・健全性のみ出力）。live key は自動変更しない。
    - **未実装（blocked）**: 既存の管理配送（command 鍵 provisioning／NATS KV）に、root とは独立に認証された trust state の配送口は無く、本 Stage は配送を接続しない。state は root 署名自体で認証されるため将来任意経路で運べるが、Agent 常駐処理への組込み、型付き handler 接続、署名付き publication 統合は別タスク。
  - **署名付き publication は未統合**: Stage 1b の `/api/app-artifacts` は署名 manifest を保存・返却しない。署名を確定 metadata と冪等比較へ原子的に結合する（manifest を ObjectInfo と同じ expected-last-subject-sequence=0 の commit に含める、または別 object なら公開順序と競合を設計する）統合は、Stage 1b の metadata / atomicity を変更しない別の限定タスクとして残す。可変な署名 metadata は許容しない。
  - **未決事項（製品判断）**: (1) `min_os_version` の identity 別の意味と互換性判定方針。(2) `package_id` の正規 registry と role / identity との対応。(3) capability 名の registry と上限。(4) 初回 publisher 鍵の生成・保管手段（本 Stage は生成コマンドを持たない）。
- **ハッシュ単体認証の禁止**: 単なる SHA-256 ハッシュの一致は完全性（破損していないこと）を証明するに過ぎず、発行元（Publisher）の正当性を証明しない。マニフェストには信頼された発行元鍵による電子署名を付与し、Agent は実行前に署名を検証する。

#### 2.13.4.4 不変性 (Immutability) の保証と実版検出
1. **新 identity 経路の不変性と互換移行**:
   - **Stage 1b 実装済み**: 明示 identity / integrity を先頭 metadata とする認証 multipart publication、metadata lookup、検証後 streamed download。独立 staging chunks の read-back 検証後、ObjectInfo + identity metadata を NATS expected-last-subject-sequence=0 で一度だけ確定する。並行同一 publish は冪等、異なる integrity は409。旧 list は有効な2 segment key のみ。直接 NATS writer を backend に隔離した構成だけ明示 opt-in で publication を有効化（既定無効）、保持設定も検査する。署名・publisher trust・alias snapshot・producer 移行は未実装で deploy-ready を主張しない。plan 永続化・参照追跡が未実装のため、新 namespace の変更・delete / GC は一律無効。クラッシュ残骸は全 writer 停止後の参照照合による保守回収のみ。詳細は [API と運用境界](app-artifacts.md)。
   - 新 identity API および CLI は、同一の `ArtifactIdentity` に対して、既存オブジェクトと異なるバイト列のアップロードを試みた場合、即座に `409 Conflict` を返して拒否する。
   - 同一 identity・bytes・署名 metadata の再アップロードだけを 200 OK (Idempotent no-op) とする。metadata の不一致も衝突として拒否する。
   - **残る proposal（Stage 1b は未署名 integrity record の atomic commit のみ実装）**: payload / manifest の途中失敗や並行 publish で不完全な組を公開しない。検証済み組だけを可視化する commit marker と conditional create を推奨する。HTTP / CLI / CI / direct Object Store principal を含む全書込経路で新 identity の上書きを防ぐ必要があり、read-back / Range / If-Match だけでは不変性を保証しない。権限制約と競合・中断・再試行を試験する。承認済み / 実行中 plan の参照先は delete / GC から保護し、旧 API からも新 identity を変更・削除できないよう分離する。保持期間・参照解除は実装時に確定する。
   - 現行 app-packages API は `<name>/<version>` を上書きする。Phase 1 は別の identity API / key namespace を追加し、旧 bare-key API と `release-extras.yml` の `publish-client-to-backend` は現行動作を維持する。agent publish/rollout も既存契約のまま維持する。一律 409 への変更は禁止。
   - 旧 Windows alias は PE 検証だけで可変 object を参照せず、その時点の bytes / digest / publisher 証跡を新 identity に snapshot する。同じ版の再 build が異なる bytes なら新 identity では拒否し、新しい版を要求する（force overwrite は設けない）。旧経路の再 upload は既存 plan を変えない。
   - CI producer と operator 手順を新 publish へ移行し、旧 consumer 不在・再 upload parity を検証後に旧 write API を廃止する。期限は Phase 7 の撤去 gate。移行中も新 CLI は bare key を直接配信しない。
2. **実版（Installed Version）の検出契約**:
   - デプロイ前後のバージョン検出は、Windows PE に依存せず各 OS ネイティブの手法を用いる。
   - **Windows**: PE `VERSIONINFO` (`ProductVersion` / `FileVersion`)
   - **macOS**: `/Applications/Kanade Client.app/Contents/Info.plist` の `CFBundleShortVersionString`、または `pkgutil --pkg-info`
   - **Ubuntu Linux**: `dpkg-query -W -f='${Version}' <package>`
   - **CLI / Backend**: 実行ファイルの `--version` コマンドライン出力、または起動時自己報告値

---

### 2.13.5 kanade deploy CLI コマンド仕様

#### 2.13.5.1 コマンド体系 (`plan` / `apply` / `status`)
デプロイ操作は安全のため「計画（Plan）」「適用（Apply）」「状況確認（Status）」の 3 段階に明確に分離する。

```sh
# 1. デプロイ計画の作成 (Read-Only, 事前検証)
kanade deploy plan <role> [target-flags] [version-flags] [safety-flags]

# 2. 計画の承認と実行 (Idempotent, 認可ゲート付き)
kanade deploy apply <plan-id> [approval-flags]

# 3. 実行状況と実版検証の確認
kanade deploy status <plan-id | exec-id>
```

#### 2.13.5.2 ターゲット指定とケースセンシティブ整合性
- ターゲットは以下のいずれか一つを厳格に指定する（相互排他）。
  - `--pc <pc_id>`: 単一端末指定。
  - `--group <name>`: グループ指定。
  - `--all`: 全フリート指定（`cli` ロールでは拒否）。
- **Case-Sensitive の厳守**:
  - Agent が NATS に登録する `pc_id`（OS ホスト名由来）は大文字・小文字が混在しており、NATS subject (`commands.pc.<id>`) はケースセンシティブである。
  - CLI は入力された `pc_id` の大文字小文字を勝手に変換（lowercase / uppercase）せず、登録インベントリと完全一致で照合する。不一致の場合は計画作成を拒否する。

**Agent scope の持続性**: 既存 `agent_config` の pc / group / global target は一回の固定 cohort job ではなく、将来の所属変更にも作用する持続 policy。plan / 承認に scope・実効 target・jitter・優先する pin とその影響を表示する。CLI の観測 timeout や plan 開始期限で policy を自動解除せず、変更は別の明示操作とする。固定 cohort だけを更新したい場合は PC scope を選ぶ。scope revision 競合と将来の enrollment / membership 変更時の platform 適合確認は追加実装 proposal とし、全 platform 成果物不足を単一 key の存在で隠さない。

#### 2.13.5.3 不変バージョン解決 (`latest` の一括解決・固定)
- `--version <ver>` に `latest` が指定された場合、または省略された場合：
  - `plan` コマンド実行時にのみ、GitHub Release API または最新の署名付きマニフェストに問い合わせ、具体的な SemVer バージョン、Git コミット SHA、成果物 URL、および SHA-256 ダイジェストを確定する。
  - 確定した具体的な値は生成される `plan` オブジェクトに完全に焼き込まれ、固定される。
  - `apply` 実行時には動的なバージョン解決を一切行わず、計画に記録された不変の成果物のみを取得・デプロイする。これにより、計画から適用までの間に新しいリリースが出ても意図しないバージョンが混入しない。

#### 2.13.5.4 Jitter, タイムアウト, デッドライン
- `--jitter <duration>`: job roles (`backend/client/cli`) の提案既定は `--pc` で `0s`、`--group` / `--all` で `15m`。Agent rollout へこの既定を適用しない。既存 fleet facade の PC 既定 `0s`、group / global の省略時は既存 scope jitter を保持する挙動を維持し、実効値・継承元を plan に表示する。
- `--timeout <duration>`: 個々の端末におけるインストール処理の最大実行時間（デフォルト: `10m`）。
- `--deadline <timestamp | duration>`: 計画の有効期限。期限を過ぎたコマンドは Agent 側で実行を拒否（exit code 125 でスキップ記録）。

#### 2.13.5.5 Read-Only Dry-Run と Fail-Closed 挙動
1. **完全な副作用なしの Dry-Run**:
   - `kanade deploy plan` は対象ホストへの一切の変更を行わない。ターゲットのインベントリ、オンライン状態、OS / アーキテクチャ、現在稼働バージョン、および必要な成果物の存在を読み取り専用で照合する。
2. **Fail-Closed 原則 (全体拒否)**:
   - 指定されたスコープ（`--all` または `--group`）内に、以下のいずれかの端末が 1 台でも存在する場合、計画の作成は**既定で全体拒否（Fail-Closed）**される。
     - オフラインまたは長期間ハートビートが途絶えている端末
     - 対象ロールに非対応の OS / アーキテクチャの端末（例: macOS 端末に対する Windows 専用機能）
     - 必要な成果物やマニフェストが存在しない端末
     - backend/client/cli の計画で、新しい型付き操作に対応していない旧 Agent。agent 計画は例外で、既存 publish/rollout に必要な platform・版・self-update 経路だけを検査する。typed_deploy_v1 未対応の Agent 自身を先に更新できなければならない
   - **明示的除外 (`--skip-unsupported`)**:
     - 運用者が明示的に `--skip-unsupported` を指定した場合に限り、非対応端末をデプロイ対象から除外し、その端末 ID と除外理由（「OS 不一致」「成果物不在」「旧 Agent」等）を計画書および監査ログに明記した上で計画作成を許可する。

#### 2.13.5.6 認可・承認・実行ゲート契約
- 以下は新規実装を要する技術 proposal であり、承認済み製品判断とは区別する。特に別人による二者承認の必須化・適用閾値は未解決。既存 `require_approval` は parse-only なので enforcement の実装前に承認済み実行を保証しない。
- デプロイの実行は CLI のローカルプロンプトだけで完結させない。
- `plan` はローカルに内容 digest 付き計画を出力する read-only 操作。`apply` の登録段階で Backend 上に一意の `Plan ID` を持つ承認待ちレコードを作る。
- `apply` の実行には、Backend の RBAC における `operator` 以上のロール権限が必要であり、高リスク操作（後述の `--wipe-db` やフリート規模の更新）には Backend 側の plan に束縛した承認ゲート（Approval Gate）を要求する案とする。二者承認 policy は上記の未解決事項。

#### 2.13.5.7 結果検証 (受理と検証済み成功の厳格な区別)
- 従来の `--no-verify` フラグは完全廃止する。
- デプロイ実行結果は、以下の二つの状態を厳格に区別して報告する。
  1. **Accepted (受理)**: Agent がコマンドを受信し、ローカルの実行キュー・台帳に記録した状態。
  2. **Verified Success (検証済み成功)**: 配置検証（identity・digest・publisher・OS 実版・所有権 receipt）が成功し、以下のロール別 postcondition を満たした終端状態。
     - Backend: 起動対象なら要求版の health と結果再投影まで検証し `running_verified`。`--start-policy never` または更新前停止なら起動せず `installed_verified`。
     - Client: 適格な既存 GUI session に再起動した場合のみ session と要求版の応答を検証して `running_verified`。未ログイン・lock・opt-out は起動を抑止し、登録内容と配置を検証して `installed_verified`（理由付き）。GUI health を待ち続けない。
     - Admin CLI: 常駐を要求せず、管理先 executable の安全な `--version` と配置検証で `installed_verified`。
     - Agent: 既存 rollout の版収束・新 heartbeat を検証する。
   両 verified 状態は成功。未検証・timeout・失敗・rollback は成功に含めない。
- **未解決 CLI proposal**: `--no-wait` を設ける場合は exec / plan ID と Accepted のみを返し、Verified Success と区別する。待機・部分失敗・延期・Accepted の終了コード、canary / wave / 同時実行数の既定は別途設計・承認し、ここでは製品判断を追加しない。
- `kanade deploy status` は各端末が Verified Success に達したことを確認して初めて全体完了を出力する。

---

### 2.13.6 Typed Native Operations (型付き Agent 操作) と Handler 契約

#### 2.13.6.1 型付き操作の設計思想 (シェルスクリプト隠蔽の排除)
任意の `.ps1` や `.sh` スクリプトを配信・実行する方式は、シェル構文の解釈揺れ、引数展開によるコマンドインジェクション、および監査ログの不透明化を招く。
特権インストール処理は、Rust で実装された型付きのエージェント操作（Typed Native Operation）とし、固定・検証された引数のみを受け取り、プラットフォームのネイティブ API やシステム標準ツールを直接呼び出す構成とする。

#### 2.13.6.2 Job schema・認可・Wire Protocol 拡張 (`TypedOperation`)

**提案（新規実装が必要）**: 現行 `Manifest.execute: Execute` は shell と script source を必須とする。Command の追加だけでは足りず、`Execute` を互換 union に拡張する。既存 shell YAML は discriminator 省略で shell として維持し、typed は `kind: typed_operation` 必須。以下は保存される job の例である。

```yaml
execute:
  kind: typed_operation
  payload_version: 1
  timeout: 10m
  run_as: system
  operation:
    type: deploy_backend
    spec:
      artifact_identity: {role: backend, version: 0.80.0, os: linux, arch: aarch64, distro_abi: musl, format: tar_gz}
      package_object_key: backend/0.80.0/linux-aarch64-musl.tar_gz
      artifact_sha256: <sha256>
      artifact_size_bytes: <positive-byte-count>
      package_id: com.kanade.backend
      manifest_sha256: <signed-manifest-sha256>
      preserve_config: true
      require_db_backup: true
      wipe_db: false
      start_policy: preserve
      repair_service: false
      firewall: preserve
require_approval: true
deploy_authorization:
  plan_id: <id>
  plan_digest: <digest>
```

- **Stage 3 実装済み（schema・検証・変換のみ。配送・実行は無効）**: `kanade_shared::typed_operation`（`ManifestExecute` / `TypedOperation` / `DeployAuthorizationRef`）と `wire::TypedCommand`。上の例との差分を明記する。
  - **判別子**: `execute.kind` 省略は従来 shell（保存済み record・直列化 bytes は不変で、shell は `kind` を出力しない）。`kind: shell` も同じ shell。typed は `kind: typed_operation` 必須。判別子が null / 未知 / 非文字列、判別子省略で typed 専用 field（`payload_version` / `operation`）が存在、typed に shell 系 field（`shell` / `script*` / `cwd` / `bypass_local_limit`、null 値を含む）が混在、typed 内の未知 field、重複 key はすべて拒否し、shell へ fallback しない。shell 側の読込は従来どおり未知 field を許容する（write 境界の `strict` は維持）。
  - **payload**: `execute.payload_version`（現在 `1` のみ。0・未来版・欠落・null は拒否）、`timeout`（1s〜1h）、`run_as`（`system` のみ）、`operation: {type, spec}`（`deploy_backend` / `deploy_client` / `deploy_cli`、spec は未知 field 拒否）。`spec` は上例の field に加え、`artifact_sha256` / `manifest_sha256` は小文字 hex 64 桁、`artifact_size_bytes` は正の byte 数、`package_id` は署名済み package manifest の ID である。`check_admission` は実際の `AdmittedPackage` と identity・SHA-256・サイズ・package ID・manifest bytes digest を完全照合するが、実行権限は付与しない。`package_object_key` は `artifact_identity` の canonical key と完全一致、identity の role は操作と一致が必要。backend は `preserve_config` / `require_db_backup` が true 必須、`firewall: managed_port` は非 0、client は `opt_out_policy: respect` と `restart_gui_session_only: true`、cli の `adopt_existing_cli` は macOS identity のみで、true の場合は `adoption_observation_sha256` が必須（false の場合は指定不可）。採用 observation の canonical JSON に `kanade/cli-adoption/v1\0` を前置した SHA-256 を計画の operation に含める。executable・path・argv を表す field は存在しない。job 直下は `require_approval: true` と `deploy_authorization: {plan_id, plan_digest}` が必須（shell job に付けると拒否）、typed は `inventory` / `emit` / `check` / `collect` / `finalize` / `feed` / `client` を持てない。`plan_id` は `[A-Za-z0-9._-]` 1–128 文字。
  - **外部期待値との束縛**: `TypedOperation::verify_binding` が、呼出側が artifact store / plan から渡す期待 `ArtifactIdentity` / `ArtifactIntegrity.sha256` / 署名済み package manifest の SHA-256 と完全一致を要求する。job の値を自分自身と比較しない。署名済み package manifest の digest（`manifest_sha256`）と job manifest の digest（Command の `deploy_authorization.manifest_digest`）は別物として扱う。
  - **Command**: `TypedCommand` は `Command` と別型で、`command_kind: typed_operation`（必須・リテラル）、`payload_version`、`deploy_authorization {plan_id, plan_digest, manifest_digest}` を持ち、`shell` / `script` / `script_object` / `cwd` を**出力しない**。そのため旧 Agent の `Command` decoder は必須 field 欠落で拒否し、新 Agent の `Command` は `command_kind` が `shell` 以外（`typed_operation`・未知・null）なら decode 自体を拒否するので、shell 実行へ既定化できない。`Command.command_kind` は `shell` の場合 serialize しないため既存 shell の署名 bytes・replay・ledger・outbox の契約は不変。`TypedCommand::from_manifest` は副作用のない検証付き変換であり、承認・capability・trust は参照しない。
  - **署名**: typed は既存 `kanade.command.v2` envelope・command-signing keyring・domain の中だけで構成し、`sign_typed_envelope` / `verify_typed_envelope`（署名 → 宛先 → 期限 → key age の順は shell と共通実装）。未署名・legacy（bare）署名 Command は typed を運べない（`kind` 不在で拒否、`Command` としても decode 不能）。shell 用 `verify_envelope` は typed envelope を署名検証後に `UnsupportedCommandKind` で拒否し、Agent は `EnvelopeRejected` として受信拒否する。別の署名方式は作らない。
  - **入口の遮断**: schema 受理は配備認可ではない。`kanade job validate` と保存済み定義の読込は typed を受理するが、`kanade job create` / `POST /api/jobs`（作成・更新）、`POST /api/exec/{job_id}` と Scheduler が共有する `exec_manifest`、backend Scheduler の発火、Agent の local scheduler（jobs watch・resync・tick）、KLP `jobs.list` / `jobs.execute` は typed を明示的に拒否または無視する。shell script resolver と shell Command builder は `ManifestExecute::require_shell()` 経由でのみ `Execute` を得るので typed に到達しない。mock 成功 handler や `typed_deploy_v1` capability の広告は追加していない。
  - **Stage 4 実装済み（Backend の認可基盤。配送・実行は引き続き無効）**:
    - **plan record**（`kanade_shared::deploy_plan` と Backend `deploy_plans` テーブル）: 不変・version 付き（`record_version: 1`）。完全な artifact identity / integrity / 署名済み package manifest digest、job manifest digest（`deploy_authorization` だけを除いた typed job を `kanade/deploy-job-manifest/sha256/v1\0` で domain 分離して hash。plan が自分自身を参照する循環を避ける）、role と operation の全 options、**大文字小文字を保持した確定 pc_id 集合**（byte 昇順・重複は拒否・登録済み agent と完全一致）、`not_before`/`expires_at`（unix 秒、幅は最大 24h）、execution policy（`single_use_per_target` のみ）に束縛する。digest は `kanade/deploy-plan/sha256/v1\0` + 全 field の canonical JSON（key 昇順・空白なし）の SHA-256 で、固定 fixture テストで規則を固定する。digest は client が事前計算でき、server は再計算して不一致を拒否する。
    - **API**（すべて `operator` 以上かつ既存 Exec 権限。新しい permission は設けない）: `POST /api/deploy-plans`（登録、常に `pending`）、`GET /api/deploy-plans` / `GET /api/deploy-plans/{plan_id}`、`POST …/approve`、`POST …/revoke`。内容の更新 API は無く、変更は新しい plan。承認者・登録者・失効者は認証済み context（`Caller`）からのみ決まり、request body の `approved` / actor は未知 field として拒否する。approve は「確認した digest と revision」を要求し、state・digest・revision・有効期限を含む**単一の条件付き UPDATE**で行う。CHECK 制約で矛盾した承認状態（承認者なしの approved 等）を表現不能にし、読み出しのたびに保存内容から digest を再計算して不一致は `corrupt` として拒否する（DB 喪失時に承認を推測復元しない。plan と単回適用 ledger は再生成可能な projection ではなく認可状態）。
    - **単回適用**: `deploy_plan_applications` の主キー `(plan_id, pc_id)` が replay を拒否する。内部の `consume_plan` は 1 transaction 内の条件付き UPDATE と ledger 挿入で原子的に消費する（失敗時は全 rollback）が、本番の配送経路は handler 障壁より前で止まるため呼ばれない（テストのみ）。
    - **共有 gate**（`deploy_plan::evaluate` / `dispatch`）: job が typed かつ有効、plan の存在・digest 再計算・job/artifact/operation 束縛、state（pending / revoked / 期限 / 未開始 / 曖昧状態）、呼出者の**現時点の**認可（operator 以上かつ Exec 権限）、target 集合の完全一致（all / group / rollout 指定は不可）、replay、各 target の preflight を順に検査する。直接 exec と Scheduler は `exec_manifest` で、Backend Scheduler・Agent local scheduler・KLP・CLI は同じ `local_refusal` で委譲された Backend 認可を持たないとして拒否する。`dispatch` は最後に**独立した `HandlerUnavailable` 障壁**を置き、承認済み・互換・全 preflight 成功でも発行・consume・成功結果の生成に到達しない（無効化する設定・feature・引数は存在しない）。publisher trust・package admission・operator 承認は別判定のまま。
    - **DB 保護**: plan の内容・digest・job・role・有効期間・登録情報は UPDATE trigger で不変にし、状態遷移と revision の単調増加、承認履歴の保持も DB で強制する。既存 consume transaction の approved 状態での revision 更新は許容する。
    - **capability**: `Heartbeat.deploy_capabilities: Option<DeployCapabilities>`（`{schema_version, handlers:[{contract, operation, os, arch}]}`。SPEC の `Vec<String>` 案ではなく構造化し、未知 token は decode 後に無視＝fail-closed）。`None` は旧 Agent・ping（unknown）、`handlers: []` は報告済み非対応で、Backend の `agents` projection と `GET /api/agents` にも省略と空を区別して保存・公開する。同一 heartbeat 由来の証拠（capability・agent_version・command_protocols・command_keys・os_family・agent_arch）と報告時刻・Backend 受信時刻を一組で保持し、capability を省略した heartbeat は証拠を更新も消去もしない。順序逆転した古い報告は無視し、現行 agent_version が証拠の版と異なれば証拠は再利用しない。Agent は handler registry（現状は空）から報告を生成するため、**handler の無い現状は空を報告し、いかなる `typed_deploy_v1`/operation/platform も広告しない**。
    - **preflight**: target の登録（pc_id 完全一致）、証拠の有無・空・鮮度（報告時刻・受信時刻とも 300 秒以内。欠落・未来時刻は不可）・版一致、OS と `agent_arch`（heartbeat と証拠と artifact）、(operation, os, arch) の handler 一致、`command_protocols` に `kanade.command.v2`、`command_keys` に Backend の現在の signer の `kid:fingerprint`、を検査し、None・空・古い・未知・不一致はすべて拒否する。Agent self-update（publish / rollout）はこの検査を一切経由せず、plan の role にも `agent` は存在しない。
  - **未実装（Task 5 以降）と未決事項**: native OS handler、Agent の `verify_typed_envelope` 接続、trust 鍵配送、CLI の `deploy plan/apply/status`、実際の配送・consume・結果検証、Scheduler が承認済み plan を一回分消費する設計。実質的に未決の選択として、plan 有効期間の上限（暫定 24h）と capability 証拠の鮮度閾値（暫定 300 秒）、**二者承認**（登録者と承認者の分離）の要否と適用閾値（現状は同一 operator による登録・承認も許す。承認者と登録者は別々に記録される）、`--skip-unsupported` に相当する部分 target 計画、plan 登録時の artifact store / 署名済み manifest との自動照合（現状は operator が渡す期待値と job の束縛のみを検証）、がある。これらが揃うまで typed を有効化しない。

- typed branch は shell / script / script_file / script_object / cwd / 任意 install_path を禁止し、未知 field・enum、role/identity 不一致を拒否する。system 実行のみ、timeout と deadline は上限を検査。型別 spec は共通の完全な identity・artifact / manifest digest に加え、backend は上記安全設定、client は `opt_out_policy: respect` と `restart_gui_session_only: true`、cli は管理済み固定配置先を持つ。後掲 Rust は Command 内の概念スケッチであり、この必須 job validation を省略しない。Force opt-out は deploy では許可しない。
- `kanade job validate/create`、`POST /api/jobs`、更新 API、保存済み manifest の読込で union の排他検証を共有する。typed の script source resolver は呼ばない。schema だけを受理して実行 gate 未実装の状態を release しない。
- `plan` の read-only 出力を `apply` が Backend に登録し、その digest（対象 pc 集合、job/manifest digest、成果物、全 spec、期限、actor）へ承認を結び付ける。§2.13.5.6 の承認は新規実装要件。現行 installer の `require_approval` は parse-only であり、それだけを信用しない。変更時は承認を失効し再 plan、適用は実行 id による冪等性を保証する。
- `POST /api/exec/{job_id}` と scheduler の共通 dispatch 境界で、対象 scope の既存 exec 権限・operator 以上・未失効の plan 承認・manifest digest・対象 inventory / capability を再検証する。通常 exec で typed job を直接指定しても plan 承認なしなら拒否。scheduler actor は承認済み一回分の plan/対象/期限を消費するだけで、新しい承認や繰返し deploy を生成しない。
- KLP のローカル job 実行・action 起動にも同じ拒否条件を置く。既存 job/action allowlist は必要条件だが特権 deploy の十分条件ではない。Backend 承認に結び付いた署名 envelope を取得できない KLP/local scheduler 経路は typed を拒否し、ローカルから Command を直構築して迂回しない。一般 shell job の既存認可は維持する。
- 検証済み Manifest.execute.operation を 1 対象 1 Command へ変換し、共通 timeout、exec/request/job id、承認 digest をコピーする。対象別 signed envelope を既存 command signer で署名し、既存 admission ledger・Outbox・execution audit に記録する。Agent は署名・宛先・expiry と job/plan digest を検証してから typed handler に入る。

既存の `kanade_shared::wire::Command` スキーマを拡張し、スクリプト実行（`script` / `script_object`）と相互排他になる型付き操作フィールドを導入する。

```rust
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct Command {
    // --- 既存フィールド ---
    pub id: String,
    pub version: String,
    pub request_id: String,
    pub exec_id: Option<String>,
    pub timeout_secs: u64,
    pub deadline_at: Option<DateTime<Utc>>,
    // ...

    // --- コマンド種別 discriminator ---
    #[serde(default = "default_command_kind")]
    pub command_kind: CommandKind, // "shell" | "typed_operation"

    // --- スクリプト実行 (command_kind == "shell" のみ有効) ---
    #[serde(default)]
    pub shell: Option<Shell>,
    #[serde(default)]
    pub script: String,
    pub script_object: Option<String>,
    pub script_object_sha256: Option<String>,

    // --- 型付きネイティブ操作 (command_kind == "typed_operation" のみ有効) ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<TypedOperation>,
    // typed の場合は必須。署名対象に含め、manifest と plan の束縛を検証
    pub deploy_authorization: Option<DeployAuthorization>,
}

pub struct DeployAuthorization {
    pub plan_id: String,
    pub plan_digest: String,
    pub manifest_digest: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
#[serde(tag = "type", content = "spec", rename_all = "snake_case")]
pub enum TypedOperation {
    DeployBackend(DeployBackendSpec),
    DeployClient(DeployClientSpec),
    DeployCli(DeployCliSpec),
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct DeployBackendSpec {
    // 各 spec の共通必須 fields（job と同じ、target_version 単独では識別しない）
    pub artifact_identity: ArtifactIdentity,
    pub artifact_sha256: String,
    pub artifact_size_bytes: u64,
    pub package_id: String,
    pub manifest_sha256: String,
    pub package_object_key: String,
    pub preserve_config: bool, // true 必須
    pub wipe_db: bool,
    pub require_db_backup: bool, // wipe の有無にかかわらず true 必須
    pub start_policy: StartPolicy, // preserve | never
    pub repair_service: bool,     // false が既定
    pub firewall: FirewallPolicy, // preserve | no_change | managed_port(u16)
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct DeployClientSpec {
    // 各 spec の共通必須 fields（job と同じ、target_version 単独では識別しない）
    pub artifact_identity: ArtifactIdentity,
    pub artifact_sha256: String,
    pub artifact_size_bytes: u64,
    pub package_id: String,
    pub manifest_sha256: String,
    pub package_object_key: String,
    pub opt_out_policy: OptOutPolicy, // Respect のみ
    pub restart_gui_session_only: bool,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct DeployCliSpec {
    // 各 spec の共通必須 fields（job と同じ、target_version 単独では識別しない）
    pub artifact_identity: ArtifactIdentity,
    pub artifact_sha256: String,
    pub artifact_size_bytes: u64,
    pub package_id: String,
    pub manifest_sha256: String,
    pub package_object_key: String,
    pub adopt_existing_cli: bool, // false が既定、macOS 固定配置先の明示採用のみ
    pub adoption_observation_sha256: Option<ArtifactSha256>, // 明示採用時のみ必須
    // 配置先は管理 receipt / role の固定 policy から解決（任意パス不可）
}
```

#### 2.13.6.3 旧 Agent との互換性確保 (Capability Preflight + Signed Envelope + Fail-Closed)
未知のフィールドを無視して不正実行する旧バージョン Agent の存在は、重大な事故を引き起こすリスクがある。これを防ぐため以下の二重防御を採用する。

1. **Capability Preflight**:
   - （Stage 4 実装済み: 実装は `Vec<String>` ではなく構造化した `Option<DeployCapabilities>`。詳細は §2.13.6.2 の Stage 4。）本提案では `deploy_capabilities` を新設し、Backend projection / inventory API にも保存する。`None` は旧 Agent / 未報告（unknown）、`Some([])` は報告済み非対応。両方とも typed 配信を拒否し、必要な `typed_deploy_v1` と OS handler token の明示列挙だけを許す。fresh heartbeat と inventory の一致を apply でも確認する。
   - 既存 `command_protocols` は別軸であり `Some` に `kanade.command.v2` が必要。`command_keys` は現在の signer の kid **と fingerprint** の一致が必要。None / 空 / 不一致はいずれも typed を拒否する。
   - **Agent self-update は例外**: deploy_capabilities や v2 対応を新たな rollout 前提にしない。既存 agent publish/rollout の検査・quarantine・版収束を維持し、旧版を typed 対応版へ先に更新して heartbeat を再確認できる。shell installer による agent 更新には変換しない。
2. **署名 envelope と fail-closed**:
   - typed Command は既存 `kanade.command.v2` envelope 内だけで受理し、既存 command-signing keyring、鍵 rotation / revocation、宛先・期限検証、admission ledger の重複排除を必須とする。enforcement が一般 command に未適用の Agent でも typed に限って署名必須。unsigned / legacy bare Command / shell stub fallback は拒否する。
   - discriminator だけでは未知 field を無視する旧 Agent の拒否を保証できない。旧 Agent への配信遮断と、旧 Command parser が required top-level field 不在で拒否する v2 envelope が防御となる。新 Agent も shell/typed 排他・未知 operation を拒否する。
3. **Publisher と command signer の役割**:
   - command signer は今回の実行権限、package publisher は配布物の出所を証明する。独立した無管理 trust root は作らない。提案は既存署名管理の管理者・配布・監査経路で、用途を `command` / `artifact-publisher` に分離した鍵 registry を管理すること。command 署名だけで任意のローカル package を publisher 認証済みとしない。
   - publisher 公開鍵の fingerprint は認証済み管理経路で Agent に事前配布し、package 同梱鍵や同じ download URL から trust を追加しない。manifest 全 identity/digest/size/requirements を canonical bytes で署名（signature 自体を除外）。kid と fingerprint を検証する。
   - rotation は新旧の有界重複期間、Agent trust 反映の確認、新鍵署名への切替、旧鍵失効の順。失効一覧を認証済み管理経路で配布し apply と install で再確認する。trust/revocation 情報の期限切れは fail-closed。失効後は保存済み plan も再承認・再署名が必要。鍵配布・失効検証が整うまで typed deploy を有効化しない。

#### 2.13.6.4 プラットフォーム別インストーラ所有権・実行モデル

| OS | 管理対象成果物 | 配置先ディレクトリ | 実行ツール / API | 所有権・権限モデル |
|---|---|---|---|---|
| **Windows** | `kanade-backend.exe`, `kanade-client.exe`, `kanade.exe` | `%ProgramFiles%\Kanade\` (バイナリ)<br>`%ProgramData%\Kanade\` (設定・DB) | Windows Service Control Manager (Advapi32 API), 原子的ファイルスワップ | `NT AUTHORITY\SYSTEM`, Administrators フルコントロール |
| **macOS Client** | `Kanade Client.app` | `/Applications/Kanade Client.app`<br>`/Library/LaunchAgents/` | `/usr/sbin/installer -pkg`, `/bin/launchctl` | `root:wheel` (アプリバンドル), ログインユーザーセッション |
| **macOS Admin CLI** | `kanade-aarch64-apple-darwin.tar.gz` 内の `kanade` | `/usr/local/bin/kanade` | Rust archive 検査・同一 filesystem の atomic rename、固定絶対パスの `--version` | `root:wheel` 0755、Kanade receipt。service / LaunchAgent 登録なし |
| **Linux (Ubuntu)** | `kanade-backend`, `kanade-client`, `kanade` | `/usr/local/bin/` または `/usr/bin/`<br>`/etc/kanade/` (設定), `/var/lib/kanade/` (DB) | Client のみ `/usr/bin/apt-get install -y <verified-local-deb>`、Backend / CLI は検証済み tar.gz の安全な展開、`systemctl` | `root:root` (Client は dpkg、Backend / CLI は Kanade receipt), systemd サービス |

**Linux Backend / CLI tar.gz handler proposal**: 現在 .deb は存在せず、生成も前提にしない。x86_64 / aarch64 musl archive を固定 staging 領域に展開し、絶対パス・`..`・symlink/hardlink・device・複数 executable を拒否、role に対応する単一 ELF の arch / digest / `--version` を確認する。root 所有の `/usr/local/bin/kanade-backend` / `kanade` に同一 filesystem の atomic rename で配置し、版・digest・配置先・元 service 状態を root-only receipt に保存する。外部 package 管理のファイルは takeover せず拒否する。Backend は既存 setup の管理 systemd unit（`deploy/linux/systemd/kanade-backend.service`、ExecStart `/usr/local/bin/kanade-backend`、User/Group `kanade`、EnvironmentFile `/etc/kanade/kanade.env`、設定 `/etc/kanade/backend.toml` と照合）を保持し、初回 enrolled-host install のみ固定 unit を設置、daemon-reload を実行する。設定 `/etc/kanade/kanade.env`、DB の実設定パス、NATS/Caddy は変更しない。設定や service ownership を確認できなければ停止前に拒否する。CLI は service 登録なし。Client .deb の postinst 等にも hidden shell deploy を移さず、privileged lifecycle は typed Rust handler が所有する。

**macOS Admin CLI tar.gz handler proposal（推奨）**: macOS 14+ Apple Silicon では既存の単体 CLI release を利用し、CLI 用 .pkg を要求しない。

- publisher 署名済み manifest と archive digest / size / identity を検証後、root:wheel 0700 の staging に Rust で展開する。絶対パス・`..`・symlink/hardlink・device・重複 entry・展開容量超過を拒否し、許容 entry は workflow の単一 `kanade-aarch64-apple-darwin`（配置名は `kanade`）のみ。Mach-O arm64 と最低 OS、固定パスでの `--version` を確認する。Client.app や Agent の installer を流用しない。
- `/usr/local/bin` は root 所有・非 group/world-writable であることを検証し、不在なら root:wheel 0755 で作成する。symlink の親・配置先や外部 package 管理（Homebrew 等）の所有物を辿って上書きしない。固定配置先は `/usr/local/bin/kanade`、receipt / journal / backup は `/Library/Application Support/Kanade/deploy/`（root:wheel 0700、各ファイル 0600）。receipt に identity、digest、publisher、mode、配置先と job/exec id を記録する。ユーザー config / credentials と PATH は変更しない。
- 配置先が不在なら新規配置、正規 receipt と digest が一致すれば更新可能。既存の手動コピーを移行する場合は `--adopt-existing-cli` を明示し、既存 executable の arch・版・digest・所有権・外部管理不在を plan に記録して承認対象とする（typed `DeployCliSpec.adopt_existing_cli: bool`、既定 false、macOS の固定配置先だけで有効）。apply 時に再照合し、差分があれば拒否する。不明な所有物は移動・削除しない。他パスの CLI は対象外として plan に表示し、実機 parity では実際の operator PATH がこの固定配置を選ぶことを確認する。
- 配置先 filesystem に root-only staging を作り、既存 binary の backup と journal を fsync してから atomic rename、mode / owner / digest と `/usr/local/bin/kanade --version` を検証する。実行中の CLI process を kill せず、既存 process は旧 inode で完了させる。常駐起動は不要で `installed_verified` が成功。検証失敗時は旧 executable と receipt を組で atomic 復元し再検証、新規配置なら今回作成したファイルだけを撤去する。復元失敗は `recovery_required`、backup を保持し有界リトライ後に停止する。

#### 2.13.6.5 排他ロック・管理プロセス停止・データ保護
1. **排他ファイルロック**:
   - デプロイ処理開始時に、OS ごとの管理ロックファイル（Windows: `%ProgramData%\Kanade\locks\deploy.lock`、Linux: `/run/kanade/deploy.lock`、macOS: `/var/run/kanade/deploy.lock`）を取得する。ロックが競合した場合は上限付き（最大 60 秒）でリトライし、超過時は延期として安全に終了する。
   - Unix の lock directory は root Agent が起動時および処理前に作成・検証する（Linux root:root 0700、macOS root:wheel 0700、lock file 0600）。macOS の `/var/run` は OS 所有の標準 `/var` → `/private/var` を通る runtime 領域であり `/run` を新設しない。標準リンク自体を一律拒否せず、Kanade 管理の `kanade` directory / lock file の symlink・不正 owner・非管理者書込みは拒否する。再起動で消えても再作成する。標準リンクの owner / target 検証と、競合差替えを防ぐ directory fd 相対の no-follow 作成・open 手順の詳細は **未解決技術 proposal** とし、実機検証を前提とする。OS の advisory lock を取得した fd を処理終了まで保持する。lock file の削除・再生成で競合を回避しない。
2. **管理対象プロセスのみの停止**:
   - 更新対象のサービス・プロセスのみをピンポイントで停止する（Windows SCM によるサービス停止、macOS `launchctl bootout`、Ubuntu `systemctl stop`）。
   - 同一マシン上で稼働している無関係なプロセスや、Agent 自身のメインプロセスを誤って巻き込んで停止させることを厳禁とする。
3. **不可侵データの保護**:
   - 設定ファイル（`backend.toml`, `agent.toml`）、SQLite データベース、ログファイル、TLS 証明書、および NATS の認証設定ファイルは、バイナリの置換によって絶対に上書き・削除されない保護領域に配置する。バイナリ配置はこれらを上書き・削除しない。DB の forward migration、明示承認された wipe、整合 snapshot 復元は §2.13.7.4–5 の別責務であり、更新全体で DB が不変という意味ではない。

#### 2.13.6.6 ジャーナル・有界リトライ・障害復旧
1. **ロールバックジャーナル**:
   - バイナリの置換前に、既存バイナリを `.old`（または OS のバックアップ領域）に退避し、操作ジャーナルを記録する。
   - 失敗時はロール別に旧成果物と起動状態を復元する。Backend は必ず §2.13.7.5 の DB とバイナリの組を復元し、旧バイナリだけを戻してはならない。
2. **有界リトライ (Bounded Retry)**:
   - 一時的なロック競合やネットワーク切断に対するリトライは最大 3 回、指数バックオフ付きで実行する。無限ループに陥るリトライは禁止する。
3. **冪等性 (Idempotency)**:
   - 要求された identity / digest が配置済みで、§2.13.5.7 のロール別検証が成功する場合は、一切のファイル置換やサービス再起動を行わず、成功（No-op Success）として即座に完了する。

#### 2.13.6.7 GUI セッション安全性と Opt-out 尊重
Client App (`kanade-client`) のデプロイ・更新における重要な安全要件：
1. **GUI セッションの厳格な適格判定**:
   - 更新後の Client App 自動起動は、**更新前に稼働していた現在もアクティブな GUI セッション**（macOS: Aqua コンソールユーザー、Linux: GNOME ログインセッション）に限定する。
   - SSH セッション、ヘッドレス環境、画面ロック中、未ログイン状態では Client App を起動しない。
2. **ユーザー Opt-out の尊重**:
   - 明示 Quit は当該 session の通常常駐の停止であり、永続 opt-out に変換しない (§2.11.7.4)。Quit 済みの Client は更新の自動復帰対象外。永続 opt-out は別設定として更新・再導入をまたいで保持する。保存・検出方法は §2.11.7.4 の未解決事項のまま。
   - 更新中の新規 launch と緊急通知 fallback は共通 launch gate と協調させる。解放後の緊急通知 fallback は §2.11.7.4–5 の別判定に従い、Quit 後も再起動し得る既存契約を維持する。deploy に opt-out の強制解除を追加しない。

---

### 2.13.7 Backend 更新特有の制御プレーン置換設計

#### 2.13.7.1 自身を配信・報告する制御プレーン更新の課題
Backend は Kanade システムにおけるジョブ配信、状態管理、および結果報告の受信用 HTTP/API を司る制御プレーン（Control Plane）である。
Backend 自身のアップデートを Kanade ジョブとして実行する場合、「**ジョブを実行・報告するサーバー自身が停止・置換される**」という循環依存と一時的な通信断が不可避となる。

#### 2.13.7.2 Agent Outbox による通信断耐性と結果再投影
1. **Agent 側 Outbox による完全保護**:
   - Agent には耐久性のある命令受理台帳（admission ledger）と結果 Outbox (`state.db`) が実装されている。
   - Backend が停止している間、Agent はローカルで型付き操作（サービス停止 → バイナリ置換 → マイグレーション → サービス再開 → ヘルスチェック）を完遂し、その実行結果をローカルの Outbox に安全に保持する。
2. **NATS 独立性と結果の再投影**:
   - NATS ブローカーは Backend とは独立したプロセスとして稼働し続けているため、Agent は Backend の停止中も NATS ストリームへ結果をパブリッシュ可能である。
   - Backend 再起動後、Backend のプロジェクター（Projector）が JetStream ストリームから未処理の結果イベントをリプレイ・再投影し、実行結果テーブル（`executions`）を自動的に最新化する。
   - CLI の `kanade deploy status` は、Backend が再起動してこの再投影が完了したことを確認して完了を報告する。

#### 2.13.7.3 設定・DB・NATS・Caddy の完全保持
Backend のデプロイでは以下を保持する。DB の forward migration、明示 wipe、失敗時の整合 snapshot 復元のみ §2.13.7.5 に従って例外的に行う。
- **設定ファイル**: `%ProgramData%\Kanade\config\backend.toml` または `/etc/kanade/backend.toml`。
- **データベース**: SQLite データベースファイル（`backend.db` および WAL/SHM ファイル）。
- **メッセージング**: NATS サーバーの設定 (`nats-server.conf`)、認証トークン/ユーザー定義、およびプロセス。
- **リバースプロキシ**: Caddy / TLS 設定ファイルおよびポートバインディング。

#### 2.13.7.4 `-WipeDb` の安全化 (明示フラグ、確認、バックアップ、再投影検証)
1. **暗黙の既定値禁止**:
   - データベースの初期化（Wipe）は、マイグレーション統合等で真に必要な場合に限られる高リスク操作である。いかなる場合もデフォルトで実行してはならない。
2. **厳格な事前バックアップ**:
   - `--wipe-db` が指定された場合、Agent は既存 DB の snapshot バックアップを作成し、退避を確認してから `kanade-backend wipe-projector` を呼び出す。
3. **非再生成データの保護**:
   - 現行 `main.rs::wipe_projector_at` は `users` / `permission_groups` / `user_passkeys` を snapshot し、schema 再生成後に復元する。承認 plan と受入試験はアカウントだけでなく permission group の feature 権限・所属・passkey credential とログインを照合する。
   - この3テーブル以外の非 event-source データが全て再生成可能と確認したわけではない。wipe 前に全データの source / retention / 復旧可否を棚卸しし、保持不能・不明なら拒否する。既存 wipe の3テーブル退避は §2.13.7.5 の全 DB snapshot・consumer checkpoint と組の復旧契約の代用ではない。
4. **再投影完了の検証**:
   - ワイプ後の初回起動時、Backend が JetStream の各ストリーム（Streams）からイベントを最初からリプレイし、プロジェクションテーブルの復元が完了するまで、デプロイ結果を「完了」と判定しない。

#### 2.13.7.5 通常更新を含む DB 互換性・復旧契約

**提案（必須の復旧要件）**: `main.rs` の boot sentinel が記す通り、forward migration 後に旧 binary だけを戻すと missing migration で起動不能になり得る。wipe の有無に関係なく全 Backend 更新で以下を実施する。

1. 新旧 binary、実設定の DB path、migration 集合、サービス状態を journal に固定する。 置換前の preflight で現在の `_sqlx_migrations` と新版が要求する集合を照合する。既知のスカッシュ非互換は停止前に拒否し、wipe が必要なら §2.13.7.4 の backup・保持対象・replay source に束縛した別の明示承認 plan を要求する。単なる `--wipe-db` 指定でこの gate を迂回しない。preflight を通過した後の予期しない migration / health 失敗は手順4の組復元へ進む。配置・DB backup・復旧に十分な空き容量と権限がなければ停止前に拒否する。
2. 成果物と復旧用旧 binary を先にローカル cache へ完全取得する。Backend を正常停止して全 SQLite writer が終了したことを確認し、SQLite backup API 等で一貫した更新前 snapshot を作成する。稼働中 DB と WAL の単純コピーは禁止。復元試験用 copy で integrity_check と migration metadata を確認し、config・権限と共に root-only に保存する。backup 失敗時は更新を進めず旧サービスへ復帰する。
3. 新版を起動して forward migration / health / projector readiness を有界時間で検証する。検証中は外部 API 書込を maintenance gate で拒否し、NATS event は保持する。migration 後に受けた非 event-source データを snapshot 復元で失う窓を作らない。この maintenance / readiness gate 自体は新規実装要件。
4. 起動・migration・health 失敗時は新版と writer を停止し、失敗 DB/WAL を隔離する。journal に紐付く互換 snapshot を正しい owner/mode と整合した sidecar 状態で atomic 復元し、旧 binary / service 設定を一緒に復元して旧版を起動・検証する。下記の consumer checkpoint を復元してから旧版の projector を開始する。旧版 health と event 再投影を確認し `failed_rolled_back` として報告する。復元にも失敗なら `recovery_required`、DB と backup を保持し再試行停止、管理者へ理由を提示する。
5. backup は health だけで削除せず、結果再投影の確認後に明示保持期間で回収する。Agent journal / outbox は Backend DB と分離し、通信復帰後に同じ exec id で結果を再送する。boot sentinel の binary-only rollback と競合しないよう deploy journal と協調させることも handler の必須タスク。


**Snapshot と JetStream consumer 位置の一体復旧 proposal（推奨、必須の追加実装）**: DB snapshot だけを復元して broker 上の進んだ durable を再利用してはならない。現行 `projector/consumer_reset.rs` は空テーブルのみを検出するため、データ入り snapshot の復元を修復しない。

- snapshot 作成前に単一 Backend の排他的 maintenance ownership を確保し、全 projector の新規 pull を停止、処理中 transaction と ACK を drain してからサービスを停止する。他の Backend instance が同じ DB / durable を進める構成では拒否する。停止後に各 durable の ACK floor の **stream sequence** `S`（consumer sequence ではない）を取得し、その後に同じ静止 DB を snapshot 化する。snapshot は S までの投影済みデータを必ず含み、commit 済み・未 ACK の重複は再配信で処理する。
- snapshot と同じ journal に stream 名・生成識別情報、durable 名・全 consumer config（filter subject を含む）、S、snapshot digest、DB migration 版を記録する。対象は現行 `consumer_reset.rs` の5経路（results、events、audit、obs_events、notification_acks）すべて。events と notification_acks は同じ stream でも別 checkpoint とする。durable 名が新版で変わる場合も更新前の対応表を保持する。checkpoint が取得できなければ更新を開始しない。
- この復旧を有効化する前提として、旧・新 projector の commit-before-ACK と再配信の冪等性（audit stream_seq、result_id 等、派生 counter と通知 ack/unack の順序も含む）を検証する。必要な修正は先行タスクとし、未検証の旧版を rollback 対象に認定しない。ACK floor より先に commit 済みの範囲が再配信されても重複や状態の逆行を生まないことを必須とする。
- rollback は新版停止と snapshot 復元後、**旧版の projector 開始前**に、記録した5経路の durable を delete/recreate し、元の filter / ACK policy を維持して `DeliverPolicy::ByStartSequence(S + 1)`（S=0 なら最初から）に戻す。consumer の delivered position や新版が進めた ACK floor は採用しない。broker 管理操作は対象 durable のみに制限した native Rust recovery coordinator が行い、ローカルの Backend service identity / credentials を利用する。資格情報を job / journal / log に載せず、必要な権限がなければ停止前に拒否する。NATS process・stream 本体・他 consumer は変更しない。
- consumer 再作成は journal の段階記録で冪等に再開し、全経路の復元が確認されるまで旧 Backend の自動再起動 / projector 開始を抑止する。boot sentinel / service recovery も同じ gate に従わせる。空テーブル向け既存 reset に任せる方式や、復元失敗を warn で無視して起動する方式は禁止する。
- rollback 開始時に各 stream の high-water mark H を固定し、S+1 から H までの該当イベントを旧 projector が処理済みとなったことを recovery readiness で検証する。更新中に新版が投影・ACK した結果・監査も必ずこの範囲から復元する。先に stream retention（max_age / max_bytes / max_msgs と purge 方針）が有界更新・復旧期間中の必要範囲を保持することを確認し、保証できない場合は更新を拒否する。途中で stream 再作成・purge・retention 欠落を検出したら `recovery_required` とし、部分的再投影を成功にしない。DB backup と checkpoint は復旧完了まで保持する。


---

### 2.13.8 移行計画・Capability Matrix・受入基準・実装タスク

#### 2.13.8.1 Capability Matrix (Operator OS × Target OS × Role)
本設計の到達目標を示す。✅ も typed は現時点の実装済みを意味せず、後述 handler / gate 実装と実機受入が必須。⚠️ は追加前提を明示する。非 Client Linux ARM の既存配備を Client の初期対象制限で削らない。

| Operator OS | Target OS / Arch | Role: Agent | Role: Backend | Role: Client | Role: CLI | 備考 |
|---|---|---|---|---|---|---|
| **Windows / macOS / Linux** | **Windows x86_64** | ✅ Self-update | ✅ Typed Job | ✅ Typed Job | ✅ Typed Job | 現行本番対象 |
| **Windows / macOS / Linux** | **Ubuntu 24.04 x86_64** | ✅ Self-update | ⚠️ Typed tar.gz handler 要 | ⚠️ GUI 有効化・deb整備要 | ⚠️ Typed tar.gz handler 要 | GNOME 制限は Client のみ |
| **Windows / macOS / Linux** | **macOS 14+ aarch64** | ✅ Self-update | ❌ (対象外) | ⚠️ Typed Job (pkg整備要) | ✅ Typed Job | 管理/試験用 Mac (Apple Silicon) |
| **全 Operator** | **Linux aarch64 (ARM)** | ✅ 既存 Self-update 維持 | ⚠️ Typed tar.gz handler 要 | 延期 | ⚠️ Typed tar.gz handler 要 | 非 Client の release 成果物を維持、ARM Agent update は撤去前必須 parity |
| **全 Operator** | **その他 Linux / desktop** | 既存 self-update の検証済み環境を維持 | 要 distro/service 検証 | 延期 | 要 distro 検証 | Client の GNOME 制限を headless role に適用しない。不明な組合せは拒否 |
| **全 Operator** | **macOS Intel (x86_64)** | 延期 | 延期 | 延期 | 延期 | サポート対象外 (告知しない) |

#### 2.13.8.2 スクリプトから CLI への段階的移行計画
既存のデプロイスクリプトから新 CLI への安全な移行は、以下のステップで進める。

```text
[現状調査・設計] (本 docs タスク)
       ↓
[Phase 1] 成果物 Identity 定義・Object Store 拡張・署名マニフェスト実装
       ↓
[Phase 2] Manifest / Command typed schema・署名 envelope 契約実装（配信無効）
       ↓
[Phase 3] Backend 承認ゲート・Capability Preflight → Agent Handler 実装 (Windows/Linux/macOS)
       ↓
[Phase 4] kanade deploy CLI (plan / apply / status) 実装・結合
       ↓
[Phase 5] Ubuntu Client GUI 対応・§2.11.7 前提検証 → Client Packaging (macOS .pkg / Ubuntu .deb)
       ↓
[Phase 6] 実機フリートでの同等性 (Parity) 検証
       ↓
[Phase 7] 運用ドキュメントの完全置換 & 旧スクリプト削除 (後続タスク)
```

- **スクリプトの維持**: Phase 6 の実機同等性検証が完了し、運用手順書が完全に置き換わるまで、`scripts/fleet-deploy.ps1` および `deploy/linux/fleet-deploy.sh` は一切変更・削除せず、機能維持する。
- **スクリプトの削除条件**:
  1. 全サポート対象 OS / Role において `kanade deploy` CLI によるデプロイが green で完了すること。
  2. 旧スクリプトと同等以上の安全性（秘密情報の非露出、実版検証、Fail-closed）が実証されること。
  3. 運用ドキュメント（`book/src/operations/` 等）が更新され、新 CLI 手順に差し替えられること。
  4. 撤去対象ファイル単位で未代替機能がないこと。`install-kanade-agent.yaml` / `scripts/deploy/agent-emergency-swap.ps1` は self-update 故障時の enrolled Agent 救済であり初期 bootstrap ではない。通常 rollout で代替したことにせず、detached swap / rollback と新 heartbeat の parity 完了まで残す（job の scheduled は更新成功ではない）。`install-kanade-nats.yaml` / `scripts/deploy/nats.ps1` は broker 更新も担う別 maintenance、`uninstall-kanade-client.yaml` は Client 除去であり四 role の通常 deploy で代替しない。bootstrap / 汎用 shell / build と共に未代替機能を残す。

#### 2.13.8.3 OS / Role 別受入テスト基準 (Acceptance Tests)
実装後の受入テスト（E2E / 手動検証）で満たすべき基準を以下に定義する。

1. **基本デプロイ・更新・No-op**:
   - 各 OS / Role に対する新規インストールおよびバージョン更新が、特権昇格プロンプト（UAC 等）なしにジョブ経由で正常完了すること。
   - 同一バージョンに対する再実行が No-op として安全に即座完了すること。
2. **下位互換性と旧 Agent 拒否**:
   - `typed_deploy_v1` capability を持たない旧 Agent に対して型付きジョブが配信されず、安全に拒否されること。
   - backend/client/cli について旧 Agent が稼働するフリートの `plan` が Fail-closed で全体拒否され、`--skip-unsupported` 指定時のみ除外記録付きで許可されること。
3. **署名・アーキテクチャ不一致の拒否**:
   - 改ざんされたバイナリ、署名不一致、またはターゲット OS/arch と異なる成果物を含む計画が、Agent の事前検証で確実に拒否されること。
4. **耐障害性と競合制御**:
   - ロックファイル競合時に有界時間待機し、デッドロックせず安全に延期終了すること。macOS 14+ の Client / CLI では `/run` 不在でも `/var/run/kanade` の安全な作成・再起動後再作成・owner/mode 検査と同時 job の排他が成立し、OS 標準 `/var` リンクを検証して許容し、Kanade 管理配下の不正 symlink と競合差替えを拒否すること。
   - ネットワーク切断や Backend 一時停止中も Agent が処理を継続し、復旧後に Outbox から正常に結果が再投影されること。
5. **Backend 更新とデータ保護**:
   - Backend 更新時、設定ファイル・SQLite DB・NATS サービスが保持され、再起動後に `/api/health` が green に復帰すること。
   - `--wipe-db` 指定時、事前バックアップが作成され、`users` / `permission_groups` / `user_passkeys` と認証・feature 権限が保持され、JetStream からの再投影が完了すること。
   - Windows / Linux x86_64 / aarch64 で通常更新の forward migration 後に health を失敗させ、旧 binary + 更新前 snapshot 復元で起動・認証・結果再投影できること。backup 不可、disk full、migration 途中失敗、復元失敗、Agent 再起動も注入し、binary-only rollback を行わず、復旧不能は recovery_required となること。maintenance 中の書込拒否と NATS 継続も確認する。
   - データ入り snapshot 作成 → 新版が結果・監査・obs・通知 ack/unack を投影して ACK → health 失敗 → DB と5経路の consumer checkpoint を復元、の順に障害試験を行う。S 以前のデータ保持、S 以降の再投影、監査の非重複、counter / 通知状態の整合を検証する。commit 後 ACK 前の crash、consumer 復元途中の再 crash、復元権限不足、retention 欠落も注入し、欠落を成功扱いせず復旧 gate が維持されること。
6. **Client GUI セッション安全性**:
   - 適格な GUI セッション（Aqua / GNOME）が存在する場合のみ `--tray` 起動され、未ログイン・SSH セッションでは起動しないこと。
   - Quit が永続 opt-out に変換されず、更新で Quit 済み instance を復帰させず、launch gate 解放後の緊急通知 fallback は既存契約で動作すること。ユーザーの opt-out 設定が更新後も尊重されること。未ログイン・opt-out・lock 中はプロセスを作らず installed_verified で終端成功し、Windows / Mac / Ubuntu の CLI も常駐せず installed_verified となること。
7. **認可・信頼・互換 API**:
   - 通常 exec / scheduler / KLP の全経路で plan 未承認・期限切れ・対象拡大・manifest 改変を拒否する。署名なし / legacy typed、未知 operation、command key fingerprint 不一致、publisher 失効を Agent でも拒否する。
   - deploy_capabilities None / 空を区別して記録し両者に typed を配信しない。一方その旧 Agent の Windows / Linux x86_64 / Linux aarch64 / macOS self-update を既存 publish/rollout で成功させ、更新後の typed 計画が通ること。
   - Phase 1 中の Windows CI bare-key 再 upload は従来通り成功し、新 identity の異なる bytes / metadata は 409、同一組は no-op、旧キー変更で既存 plan の digest が変わらないこと。並行 publish・payload / manifest の途中失敗・全書込経路・参照中 delete / GC を試験し、不完全な組や差替えが resolver に見えないこと。
8. **OS / role の追加前提**:
   - macOS CLI は arm64 tar.gz から固定 `/usr/local/bin/kanade` に新規配置・更新・No-op できること。実行中 CLI を kill せず、署名 / arch / archive traversal 不正と Homebrew 所有物を拒否する。明示採用なしの手動配置拒否、採用時の digest 再照合、置換後検証失敗の旧 binary / receipt 復元、ユーザー config 保持を試験する。
   - Ubuntu x86_64 Client は実際の GUI build で手動起動・KLP・通知・session/opt-out を §2.11.7 に照らして検証し、shim を包んだ .deb を拒否する。Mac は ad-hoc app/pkg の receipt と GUI 境界、Windows は既存 protocol/shortcut の回帰を検証する。延期機能は別途未実装のまま明記する。
   - Linux x86_64 / aarch64 Backend・CLI の tar.gz で traversal・外部管理所有権・既存 unit/config 保持・停止状態保持・No-op を確認する。Windows Backend では repair/firewall/start policy の明示指定と既定保持を検証する。全 operator OS から各対象を計画・適用でき、別 repository / ローカル成果物と拒否理由も parity 表で確認する。

#### 2.13.8.4 依存関係順の小さな実装タスクリスト (Phase 1 〜 Phase 6)
本仕様に基づく実装を、後続の開発者が安全かつインクリメンタルに進められるよう、依存関係順のタスクに細分化する。

- [ ] **Task 1: 成果物 Identity & Object Store 拡張 (Shared / Backend / CLI)**
  - [x] Stage 1a: 検証付き共有 `ArtifactIdentity` / integrity、明示 target 許可表、canonical app key の純粋な生成・解析、未認証 legacy Windows 候補検査（deploy / endpoint / storage には未接続）
  - `OBJECT_APP_PACKAGES` の新キー体系 (`<role>/<version>/<os>-<arch>-<distro_abi>.<format>`) 対応
  - レガシー Windows bare key の PE 検証付き安全な移行ロジック実装
  - 新 identity API の不変性、旧 API 上書き動作の並存、CI producer 移行と alias snapshot の実装
- [ ] **Task 2: 署名付きマニフェスト生成と Publisher 検証 (Shared / CLI / Agent)**
  - [x] Stage 2a: `package.manifest.json` のスキーマ定義、決定的署名 bytes、`kanade app sign-artifact` / `verify-artifact` / `publisher-trust` のオフライン実装（§2.13.4.3。managed trust・Agent 検証・署名付き publication 統合は未実装）
  - [x] Stage 2b: 管理者 pin の trust root、署名付き trust state（active/revoked/rotation/freshness/anti-rollback）、原子的永続化、Agent 側 admission 検証（§2.13.4.3）。管理配送の統合、typed handler 接続、署名付き publication は未実装
- [ ] **Task 3: Command Wire Protocol 拡張 & Typed Native Operations 定義 (Shared)**
  - [x] Stage 3: typed job / command の schema・検証・変換、Manifest.execute 互換 union、v2 envelope 内の署名・旧 Agent 拒否、全 manifest 読込・CLI・backend・Agent の入口での typed 明示拒否（§2.13.6.2。配送・実行は無効のまま。plan 承認・capability・handler・trust 配送は Task 4 以降）
  - `Command` struct への `command_kind` および `TypedOperation` フィールド追加
  - Manifest.execute の互換 union・CLI/API/read validation・typed source resolver 分岐・Command 変換を実装（Task 4 の gate 接続前に定義）。既存署名 v2 envelope 必須化と ledger/outbox 接続、旧 Agent の v2 拒否を検証
- [ ] **Task 4: Backend 実行ゲート & Capability Preflight 実装 (Backend)**
  - [x] Stage 4: Heartbeat.deploy_capabilities（構造化 Option）と projection/API、None / 空 / 古い / 未知の拒否、既存 command_protocols / command_keys 照合、Agent self-update の適用除外、plan の登録・永続化・承認・失効 API、通常 exec / scheduler / KLP / local の共有 gate・plan digest 検証と typed 配信遮断（§2.13.6.2。承認済みでも handler 障壁で配送は常に無効。native handler・CLI・trust 配送・署名付き publication 統合は未実装）
  - 残り: Task 5 の native handler が実機検証されるまで feature 無効のまま維持する
- [ ] **Task 5: Agent プラットフォーム別 Typed Handler 実装 (Agent)**
  - Windows: SCM 制御、原子的バイナリスワップ、レジストリ保持
  - [x] Stage 5a（macOS Admin CLI、オフライン／ローカルの installer 基盤のみ）: `crates/kanade-agent/src/native_install/`。arm64 tar.gz の有界な検査（単一 member `kanade-aarch64-apple-darwin`、traversal / link / 余分な member / set-id / 展開量超過を拒否）、thin arm64 Mach-O と最低 OS 14 の検査、独立再ハッシュ済み bytes + 現在の package admission + 署名 manifest 束縛を満たす入力のみ受理、plan 承認（`Approval`）は admission と別の前提、`<install_dir>/kanade` と `<deploy_dir>` の journal / receipt / backup / lock、明示採用（承認時 observation を apply 時に完全再照合）、dirfd 相対 no-follow 操作と atomic swap（`renameatx_np`/`renameat2`）、有界 flock、永続 phase journal と phase ごとの再起動回復、rollback / `recovery_required`（凍結・backup 保持・再試行しない）、同一 identity + 検証済み内容の no-op。実行するのは固定 argv・空環境・時間／出力上限付きの `--version` のみで、`installed_verified`（ファイル・receipt の検証）と実行観測（`ExecutionObservation`）は別の結果として返す。Production 固定パスは定数として保持し、installer API は一時 root 内に限定する。`Approval` は既存 stage-4 `evaluate` を呼出し、操作・job・対象を照合する。採用 observation の digest は `DeployCliSpec.adoption_observation_sha256` を通して承認計画に束縛する。macOS は fd の拡張 ACL を検査して拒否する。試験は一時 root と fake `--version` adapter のみ。lock は SPEC 草案の `/var/run/kanade` ではなく root 専用 `<deploy_dir>/cli.install.lock` に置く（本スライスの仮定）。
    - **未配線（意図的）**: `deploy_handlers::implemented()` は空のまま、heartbeat は capability を広告しない。`tested_but_not_dispatch_ready()` は「installer が実装・試験済み」という事実だけを示し、`END_TO_END_DEPLOY_READY` は false。backend の typed 配信遮断、署名 command / replay / approval / result の統合は変更しない。
    - **次の統合前提**: Agent の `verify_typed_envelope` と `AuthorizedDeployment` の接続、署名付き artifact + manifest の publication と取得、結果の ledger / outbox 送出、frozen journal の operator 解除手順、実機（root:wheel、ACL / quarantine xattr、macOS 14+ の実 Mach-O `--version`、`/usr/local/bin` の実 PATH 優先）の受入。
  - Stage 5b（macOS Client pkg、隔離したローカル基盤）: `native_install/client_pkg/` に承認・publisher admission・固定 pkg 構造の検査、OS receipt と bundle の照合、pkg 専用 journal・復旧状態機械を追加。実 transaction は fake adapter で試験し、native の BOM / ad-hoc signature 検査は実 package で検証する。安全な launch gate がないため native 導入は延期し、GUI 復帰は unsupported。production 配信・capability・LaunchAgent・`/var/run/kanade` lock は未実装のまま。[実装範囲と受入待ち](native-client-pkg-handler.md) を参照。
  - Linux: Backend/CLI 用 x86_64・aarch64 tar.gz 展開・receipt・atomic swap・固定 systemd unit、Client 用 apt/dpkg の独立 handler を実装
  - 排他ロック、管理プロセスのみの停止、配置/稼働別の成功検証、ジャーナル、有界リトライを実装
  - Backend の整合 DB snapshot・maintenance/readiness gate・DB/binary 組復元・5経路の静止 checkpoint / consumer 再作成 / replay readiness・boot sentinel 協調を独立した小変更で実装後、handler に結合
- [ ] **Task 6: GUI セッション適格判定 & Opt-out 保持 (Client / Agent)**
  - macOS Aqua / Linux GNOME ログインセッションの保守的判定ロジック実装
  - ユーザー別 opt-out 設定の永続化と更新時引き継ぎ
- [ ] **Task 7: kanade deploy CLI コマンド実装 (CLI)**
  - `kanade deploy plan` (ターゲット検査、不変バージョン解決、Dry-run、Fail-closed)
  - `kanade deploy apply` (認可ゲート連携、ジョブ発行)
  - `kanade deploy status` (実版検証、ヘルスチェック照合)
- [ ] **Task 8a: Ubuntu Client GUI 前提 (Client / CI)**
  - Linux exit-fast shim に代わる Ubuntu 24.04 x86_64 GNOME 対応 GUI entrypoint と GNU/WebKitGTK build を整備
  - §2.11.7 の未実装 session / KLP / 通知契約を個別検証。GUI と Task 6 の受入前に Linux packaging を完了扱いしない
- [ ] **Task 8b: Client Packaging パイプライン整備 (CI / Scripts、Linux は 8a 依存)**
  - Apple Silicon macOS 向け `Kanade Client.app` バンドルおよび `.pkg` 生成パイプラインの構築
    - **packaging slice 実装済み（activation なし）**: `scripts/build-client-macos-pkg.sh` が未署名・payload-only の `client/<version>/macos-aarch64-darwin14.pkg` を生成し、構造検証と負例試験を `client-macos.yml` で実行する（手順は `crates/kanade-client/MACOS.md`）。LaunchAgent / session helper / opt-out は未実装のため自動起動は有効化しない。残: installer 署名・notarization、clean macOS 14 での実 install / receipt / Gatekeeper / GUI / KLP 受入、型付き pkg handler、リリース公開（kata 管理 workflow の上流対応）
  - Ubuntu 24.04 x86_64 向け `.deb` パッケージ生成パイプラインの構築
- [ ] **Task 9: 実機フリートでの同等性検証 (Acceptance Testing)**
  - Windows, Ubuntu, macOS 実機環境での E2E 受入テスト実施
  - Backend 制御プレーン置換および Outbox 再投影の動作確認
- [ ] **Task 10: 運用ドキュメント差し替え & 旧スクリプト削除**
  - `book/` および `docs/` 内の運用マニュアルを `kanade deploy` 手順へ改定
  - 同等性が実証された `scripts/fleet-deploy.ps1` および `deploy/linux/fleet-deploy.sh` の削除

### 2.13.9 復元代替案との照合記録 (2026-10-18)

元タスク4167の保存済み `kanade-deploy-design.patch`（SHA-256 `a9ec040b6b886454f93eb5f5d85d25e287484d56363adc053faba01d809bbcfa`、431 additions / 2 deletions、3文書）は変更せず外部添付として保持。基点 `ed11a224878725120b0056de0b193d5b773c0945` の3ファイルを scratch に抽出し、`patch -p1 --batch` で全 hunk が成功した復元文書の §2.11.8.1–10 を比較した。原本は本リポジトリにコピーしない。

元 instruction、`impl-B.out` の復旧生成・適用検証記録、run `20261017-192308-2c31` の `run.json` を確認した。未コミット案は files=0 / commits=0 / empty=true、verified_noop は null、judge_skipped=true / judgements=[] であり、比較による設計棄却や検証済み no-op の証拠ではない。正本は PR #1618 の `098daa38` に着地した本 §2.13 とする。元 run の review / fix 記録にある認可・旧 Agent 拒否・Linux ARM / GUI 前提・署名鍵管理・Mac CLI・DB snapshot / consumer checkpoint 復旧の追補を維持する。

照合時の HEAD / ローカル `origin/main` は `0ba31158e9f16c75e142982dc12e8b440ca71cfa`。`git fetch --no-write-fetch-head origin main` は DNS 解決失敗で最新 upstream 一致は未確認。PR #1618 後のローカル履歴は release #1619 / #1621 と macOS Screen Recording onboarding `b43fc9c5` であり、この機能を Client の pkg / GUI session 配送の完成とは扱わない。`3b7948fc` の Agent embedded metadata 検査は現在の publish に存在し、旧案の一般的な magic / version 照合だけへ戻さない。

| 差分 | 判断・根拠と反映先 |
|---|---|
| 全 OS CLI / enrolled only / jobs / typed native / 既存 Agent rollout、Client 初期範囲 | **既存で充足**。§2.13.1 の確定判断を維持。bootstrap one-liner・独立 updater 不採用も変更しない |
| 旧 Client updater 記述と現状調査 | **採用・修正**。§2.11.6 は履歴を残して置換済みと注記、§2.11.7 は Mac GUI と Linux shim を分離。README / book は未実装仕様への案内を加え、現行手順を維持 |
| HTTP endpoint / SourceUrl、Agent jitter | **採用**。現行 CLI `cmd/{app,agent,exec,job}.rs` と fleet script を照合し、§2.13.1–2 / .5 と README を修正。job の15m案を Agent scope 継承へ適用しない |
| platform family を既存 name に符号化 | **不採用（現時点）**。2 segment API を再利用できる利点はあるが family grammar / revision の恒久化を伴う。現行 identity tuple と別 API 移行を維持し、bucket / key の矛盾だけ解消。Stage 1a で必須 ABI / canonical grammar を確定。revision の業務対応・namespace / API は未解決 |
| 全書込経路の不変性、競合 publish / 部分失敗 / GC 保護 | **採用 proposal**。Stage 1b は新経路の条件付き commit / integrity API と旧 list の2 segment 限定を実装。署名 trust / plan 参照 GC 保護は未実装、新 namespace mutation は無効。旧 CI upload 契約を維持。旧 Windows alias は AMD64 / 実版 / provenance と immutable snapshot が必要 |
| 通常 plan で upload / Backend 保存、dry-run だけ read-only | **不採用**。現行の全 plan read-only と apply 登録を維持。ローカル入力の publish は apply、旧案の CLI 綴り・節構成への全面置換もしない |
| 別 wire kind / shell stub 代替、publisher trust | **既存を維持**。現行 §2.13.6 の v2 envelope 必須・preflight・鍵 fingerprint / 失効・全 dispatch 認可が具体的。旧案の別 kind や stub へ変更する必要はなく、stub fallback は採らない |
| Quit / opt-out / 緊急 fallback | **採用**。§2.11.7.4 の確定契約を優先し §2.13.6.7 / 受入を整合。永続 opt-out 保存方式は未解決 |
| Backend 復旧・不可侵データ | **部分採用**。wipe の保持対象は `main.rs::wipe_projector_at` の3テーブルに訂正。migration preflight と実行後失敗を分離。旧案の「互換保証がなければ自動復旧しない」へ後退せず、現行の maintenance・全 DB snapshot・5 consumer checkpoint・DB/binary 組復元を維持。他の非再生成データ調査は未完了 |
| Mac lock / native package ownership | **部分採用**。既存 Mac CLI handler と `/var/run` を維持し、OS 標準リンクと管理配下 symlink を区別。安全な open 手順は未解決。pkg/deb の復旧手順・依存 transaction は adapter 実装時に検証 |
| Agent scope の持続と将来 membership | **採用**。`cmd/agent.rs` / `agent_config` は持続 target を設定する。固定 job cohort と区別し、timeout で scope を clear しない。競合検出は追加 proposal |
| emergency swap / NATS maintenance / Client uninstall | **採用**。現行ファイルに機能が存在し、四 role 通常更新だけでは代替できない。§2.13.8.2 のファイル単位の撤去 gate に追加 |
| 二者承認、canary 1 / 同時5、具体的終了コード、no-wait | **未解決 proposal**。有用だが operator 承認済みの数値・policy ではない。Accepted / Verified の区別は維持し、詳細決定を保留 |
| Release 実物・全 OS parity | **未検証を維持**。workflow の archive 内部名は target suffix 付きと訂正。公開 asset の実物・publisher 証跡・実機挙動は未確認で、列挙を配布可能性の証明にしない |

検証は復元パッチ適用・変更文書の UTF-8 / LF / fence / 参照先・`git diff --check` とコードの静的照合。HTML はローカル mdBook 0.5.4 の en / ja 設定で生成できたが、gettext 不在を optional にした検査で翻訳と CI 固定版 0.4.40 の検証ではない。所定の gate / pre-PR review の成否は PR 記録に別記し、実機・公開 Release 実物の受入とは区別する。

これは設計文書の reconciliation であり、実装・配備・script 削除・元 run の再実行・task 登録を行わない。受入項目は将来の検証条件であり、本変更で合格した試験ではない。

---

**END OF SPEC**
