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

本節の配置・配布・自己アップデートは **Windows 専用**。Linux / macOS は §2.11.7 を参照 (未設計)。

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

### 2.11.7 Client App 配置 (Linux / macOS) — 後続段階で決定

KLP の仕様 (§2.12) は 3 OS 分を先に定める。一方で Client App 自体の Linux / macOS 配置は**後続段階**であり、本節では決めるべき項目を列挙するだけで設計はしない。

- インストール先 (バイナリ・アプリバンドルの配置、権限、署名 / notarization の要否)
- ユーザーごとの config / log / cache のパス (XDG Base Directory / `~/Library/...` のどちらに置くか)
- 自動起動 (macOS: LaunchAgent、Linux: XDG autostart) と、ユーザー初回ログイン時の展開方法 (Windows の ActiveSetup 相当)
- 自己アップデートの swap 手順 (Agent は root で動くため特権昇格は不要の見込みだが、起動中バイナリの停止・差し替え・再起動の手順は未定)

## 2.12 KLP (Kanade Local Protocol)

Client App ⇄ Agent の IPC プロトコル。OS の認証機構をそのまま使い、追加の鍵管理を不要にする。「**エンドユーザーに NATS credentials を渡さない**」 を確実に守るための層。

**OS 間の共通性**: Framing (§2.12.2)、JSON-RPC (§2.12.3)、Method 名前空間 (§2.12.5)、Handshake (§2.12.6)、Error Model (§2.12.9) は **OS 非依存で Windows / Linux / macOS 共通**。OS ごとに異なるのは Transport の実体 (§2.12.1) と、接続元 identity の取得方法 (§2.12.4) のみ。

**実装状況**: 現行実装は Windows のみ (`crates/kanade-agent/src/klp` は `cfg(windows)`、他 OS では `compile_error!`)。Linux / macOS の記述は後続実装のための仕様であり、本仕様の策定時点でコードは変更しない。

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
| session | `OpenProcessToken()` → `GetTokenInformation(TokenSessionId)` | peer pid の logind session (`sd_pid_get_session`)、取得不能なら `None` (提案、§2.12.4.4) | `None` (提案、§2.12.4.4) |

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
  "data": {"kind":"Unauthorized","detail":"uid 998 is below the minimum interactive uid 1000"}
}}
```

`detail` には拒否理由 (上記 1〜5 のどれか) を入れる。Client の扱いは §2.12.10。

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
- 現状の差分 (注記): 現行の `notifications.ack` handler は SID と通知 ID を検証して保存するだけで、通知が自分宛 (pc / group / all) かは確認していない。下記の「自分宛のみ」は仕様上の制約であり、実装との差を後続で埋める。

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

#### 2.12.4.4 未決事項 (提案 — オーナー確認待ち)

**(1) 同一ユーザーの別接続による kill / ack の扱い — 提案: 現行の「自接続限定」を維持**
- `jobs.kill` は引き続き自接続の run のみ。Client 再起動後は既存 run を `jobs.list` / `jobs.progress` で見ることはできても kill はできない。run は実行中の副作用を持つため、同じ UID の別プロセスに run を乗っ取られる余地を作らない。
- `notifications.ack` / `unack` と `support.unlock` / `lock` は元々接続限定ではなく、同じ `user_key` の別接続から更新できる (再起動・多重起動で状態が引き継がれる)。
- **オーナーがこの提案を覆す場合** (同一 `user_key` なら kill 可とする等) は、本節の本文と §2.12.4.3 の依存表 (kill の行を「接続」から「`user_key`」へ) の両方、および §2.12.5 の `jobs.kill` 行を直す必要がある。

**(2) Session の等価物 — 提案**
- Session は認可に使われていない (§2.12.4.2) ため、等価物がなくても Linux / macOS の KLP は成立する。
- Linux: peer pid の logind session (`sd_pid_get_session`) を**任意情報**として保持。取得できなければ `None`。
- macOS: `None` とする。SCDynamicStore の console user は UID の一致しか示さず、同じ UID の SSH 接続や fast user switching の裏セッションを区別できないため、console 一致や audit session id による判定は誤判定のリスクから採らない。
- SSH / ヘッドレス等で session が無くても**接続は拒否しない**。適格なアカウントなら KLP を許可し、GUI の不在を認可の拒否理由にしない。
- toast / `session_supervisor` に相当する機能は、peer の Session とは別の「**GUI session の在席・切替・ロック状態**」責務として分離する。Linux は logind のアクティブなグラフィカル session、macOS は SCDynamicStore の console user を基準にする。GUI が確認できなければ toast を出さず、通知は `notifications.list` の履歴から回復する。この責務の実装は後続段階。

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
| `jobs.kill` | req-rep | 自接続で投げた run の停止 (同一ユーザーの別接続からは不可 — 提案、§2.12.4.4) |
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
| 接続直後に `id: null` の `-32000 Unauthorized` を受けた (§2.12.4.1 の拒否) | **再接続しない**。認可条件を満たさないアカウントなので、バックオフで再試行しても結果は変わらない。トレイ等で理由 (`detail`) を表示するに留める |
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
| KLP listener (Pipe / UDS) | `crates/kanade-agent/src/klp/server.rs` (現行は Named Pipe のみ。UDS は後続) |
| `PeerIdentity` 定義と OS peer 認証 | `crates/kanade-agent/src/klp/auth.rs` (`cfg(windows)`: Pipe → token / `cfg(target_os = "linux")`: `SO_PEERCRED` / `cfg(target_os = "macos")`: `LOCAL_PEERCRED` + `LOCAL_PEERPID`) |
| 接続認可 (UID 下限・非対話アカウント拒否) | Unix 側の `klp/auth.rs` (後続実装)。Windows は `klp/security.rs` の SDDL |
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
- [ ] Agent: KLP listener (Windows: Named Pipe / Linux: UDS `/run/kanade/agent.sock` / macOS: UDS `/var/run/kanade/agent.sock`) + OS peer 認証 (Windows token / Unix peercred) + subscription manager。現行実装は Windows のみ、Unix の listener・認可は後続
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
- [ ] (後続段階) Linux / macOS の Client 配置・自動起動 (LaunchAgent / XDG autostart)・自己アップデート swap (§2.11.7)
- [ ] Client App 自己アップデート (`client_releases` Object Store bucket) (Windows。Unix の swap は §2.11.7)

### Sprint 8.5: Client App 拡張機能 (順次)

- [ ] パスワード期限通知 (AD 連携 or Windows API)
- [ ] VPN / プロキシのワンクリック再接続
- [ ] フィッシング報告ボタン (`events.security.report.{pc_id}`)
- [ ] セルフサービスソフトウェアカタログ (`category: catalog`)
- [ ] 言語切替 (ja/en) + ダーク/ライトテーマ

---

**END OF SPEC**
