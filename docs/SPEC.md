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
  - [2.6 バージョン管理と緊急停止 (3層防御)](#26-バージョン管理と緊急停止-3層防御)
  - [2.7 セキュリティ](#27-セキュリティ)
  - [2.8 信頼性・可用性](#28-信頼性可用性)
  - [2.9 監視・運用](#29-監視運用)
  - [2.10 デプロイ構成とサイジング](#210-デプロイ構成とサイジング)
  - [2.11 リポジトリ・ディレクトリ構成](#211-リポジトリディレクトリ構成)

---

# Part 1: 概要設計

## 1.1 システム概要

数千台規模の Windows PC を、AD 非依存で一元管理する自前基盤。
インベントリ採取・全体配信・緊急コマンド実行の 3 用途を、単一の pub/sub 基盤で吸収する。

商用 (Intune, Tanium 等) を導入せず自作する場合の参照アーキテクチャ。

## 1.2 主要ユースケース

| # | 種別 | 例 | 性質 |
|---|------|----|----|
| ① | 定期情報採取 (インベントリ) | 電源 ON/OFF、サインイン、HW/SW 情報、ネットワーク・ドライバ情報 | 周期的 + イベント駆動 |
| ② | 定期全体配信 | パッチ適用、SW アップデート、ファイル配信、設定変更 | 計画的、Wave 配信 |
| ③ | 一時的な緊急処置 | 特定端末への任意コマンド、緊急情報採取 | 同期 Request-Reply |

## 1.3 設計方針

1. **pub/sub + 永続化ストリーミング**
   ファイルベースやクラサバ (Server→Client push) ではなく、エージェント発の outbound 接続 + メッセージブローカー方式を採用。FW/NAT フレンドリーかつ、ファンアウト・ファンインがネイティブ。

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
| スケジューラ | `tokio-cron-scheduler` | cron 形式 |
| 設定読み込み | **TOML + `yukimemi/teravars`** | `[vars]` 自己参照 + `is_windows()` で OS 分岐 |
| ロギング | `tracing` + `tracing-subscriber` | Windows ではイベントログ併用 |

**OS 選定の指針**:
- 既存運用が Windows Server 中心なら **Windows Server** で問題なし。NATS / Rust / SQLite すべて Windows で完全動作
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

PoC を Phase 1 で立ち上げ、運用が乗ってから Backend を生やすのが現実解。NATS のメッセージ契約 (Subject 設計) を最初から後方互換に保つことで、後段の追加で破壊変更を避ける。

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

## 2.2 NATS Subject 設計

### 2.2.1 配信系 (Backend → Agent)

| Subject | 用途 |
|---|---|
| `commands.all` | 全台向けコマンド |
| `commands.group.{group_name}` | グループ単位コマンド (canary, wave1 等) |
| `commands.pc.{pc_id}` | 個別端末向けコマンド |
| `commands.deploy.{job_id}` | 配信ジョブ (バージョン管理対象) |

### 2.2.2 報告系 (Agent → Backend)

| Subject | 用途 |
|---|---|
| `inventory.{pc_id}.{category}` | インベントリ (category: hw, sw, net, driver 等) |
| `events.{pc_id}.{type}` | リアルタイムイベント (power.on, session.signin 等) |
| `results.{request_id}` | コマンド実行結果 |
| `heartbeat.{pc_id}` | 死活確認 (定期) |

### 2.2.3 制御系

| Subject | 用途 |
|---|---|
| `kill.{job_id}` | 特定ジョブの即時停止 |
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

`DEPLOY` のみ `MaxMsgsPerSubject=1` + `DiscardPolicy::Old` で「同一 job_id の最新版のみ保持」を実現する。

### 2.3.2 JetStream KV (現在状態)

| KV Bucket | キー | 値 | 用途 |
|---|---|---|---|
| `agents_state` | `{pc_id}` | JSON (latest inventory) | 端末の最新状態 |
| `deployments` | `{deploy_id}` | JSON (progress) | 配信ジョブ進捗 |
| `groups` | `{group_name}` | JSON (member pc_ids) | グループ定義 |
| `schedules` | `{schedule_id}` | JSON (cron, target) | スケジュール定義 |
| `script.current` | `{cmd_id}` | バージョン文字列 | 現行有効バージョン |
| `script.status` | `{cmd_id}` | `"ACTIVE"` / `"REVOKED"` | 緊急停止フラグ |
| `agent_config` | `schedule.{name}` | JSON | Agent への動的設定配信 |

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

## 2.4 命令定義 (YAML スキーマ)

### 2.4.1 ジョブ定義 (jobs/*.yaml)

```yaml
id: cleanup-disk-temp           # 必須、cmd_id として使用
version: 1.0.1                  # 必須、semver
description: "Temp ディレクトリのクリーンアップ"

target:                         # いずれか必須
  groups: [wave1, wave2]        # グループ指定
  # pcs: [PC1234, PC5678]       # 個別指定
  # all: true                   # 全台

execute:
  shell: powershell             # powershell / cmd / wsl
  script: |                     # インライン (small)
    $temp = [System.IO.Path]::GetTempPath()
    Remove-Item "$temp\*" -Recurse -Force -ErrorAction SilentlyContinue
    Write-Output "cleaned: $temp"
  # script_file: scripts/cleanup.ps1  # 別ファイル参照 (large)
  # script_object: installers/setup.ps1  # Object Store から取得
  timeout: 600s
  jitter: 5m                    # 0 〜 5 分のランダム遅延
  run_as: system                # system / user

rollout:                        # 配信戦略 (省略可)
  strategy: wave
  waves:
    - {group: canary, delay: 0m, size: 50}
    - {group: wave1,  delay: 30m}
    - {group: wave2,  delay: 60m}

on_failure:
  retry: 2
  alert: slack                  # slack / email / none

require_approval: true          # 本番配信時に承認必須
```

### 2.4.2 グループ定義 (groups/*.yaml)

```yaml
id: wave1
description: "第 1 波対象"
members:
  static: [PC1234, PC1235, PC1236]   # 静的リスト
  dynamic:                            # 動的クエリ (SQLite に対する)
    sql: |
      SELECT pc_id FROM agents
      WHERE os_version LIKE 'Windows 11%'
        AND last_seen > datetime('now', '-7 days')
```

### 2.4.3 スケジュール定義 (schedules/*.yaml)

```yaml
id: daily-inventory
cron: "0 0 2 * * *"             # 毎日 2:00
job: inventory-full
target:
  groups: [all]
enabled: true
```

### 2.4.4 Agent / Backend 設定ファイル (TOML + teravars 変数展開)

Agent / Backend 自体の起動設定は **TOML + Tera テンプレート構文** で記述し、`yukimemi/teravars` crate (v0.1.5+) で読み込む。teravars は `[vars]` セクションの自己参照解決 + `system.*` context + クロスプラットフォーム判定 (`is_windows()` / `is_linux()`) + multi-file merge を一気通貫で提供する。

(teravars は `shun` / `rvpm` / `todoke` / `yui` / `spyrun` の重複パターンを切り出した crate)

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

特に `is_windows()` / `system.host` / `env(default=...)` の 3 つで、Win/Linux 両対応の単一 agent.toml が現実的に書ける。これが teravars 採用の最大のメリット。

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

Backend 内の `tokio-cron-scheduler` が、`schedules` KV の定義に基づき定刻 publish。

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

## 2.6 バージョン管理と緊急停止 (3層防御)

### 第1層: Broker 滞留メッセージの置換

JetStream Stream の `DEPLOY` を `MaxMsgsPerSubject=1` + `DiscardPolicy::Old` で構成。同一 `commands.deploy.{job_id}` の旧版は新版 publish 時に自動削除される。
オフラインだった Agent が復帰した際、必ず最新版だけ受信する。

```rust
StreamConfig {
    name: "DEPLOY".into(),
    subjects: vec!["commands.deploy.>".into()],
    max_messages_per_subject: 1,
    discard: DiscardPolicy::Old,
    ..
}
```

### 第2層: 実行直前の version 照合

Agent は実行直前に必ず `script.current.{cmd_id}` KV を参照し、受信した version と一致しない場合は実行をスキップする。

```rust
let current = kv.get(&format!("script.current.{}", cmd.id)).await?;
if cmd.version != current {
    log::warn!("skip stale {} (current: {})", cmd.version, current);
    return Ok(());
}

let status = kv.get(&format!("script.status.{}", cmd.id)).await?;
if status == "REVOKED" {
    log::warn!("skip revoked command {}", cmd.id);
    return Ok(());
}

execute(cmd).await
```

### 第3層: 実行中の緊急停止

Agent は子プロセス起動と同時に `kill.{job_id}` を subscribe し、kill 通知受信で子プロセスを SIGKILL する。

```rust
let mut child = Command::new("powershell").args(...).spawn()?;
let mut kill_sub = client.subscribe(format!("kill.{}", job_id)).await?;

tokio::select! {
    status = child.wait() => { /* 正常終了 */ }
    _ = kill_sub.next() => {
        child.kill().await?;
        log::warn!("killed job {} by remote signal", job_id);
    }
}
```

**重要**: kill signal 経路は MVP 段階から必ず仕込むこと。後付けは既存スクリプト全てに kill 経路を埋め込む作業になり困難。

### まとめ

| いつ | 仕組み | 状態 |
|---|---|---|
| broker に滞留 | `MaxMsgsPerSubject=1 + DiscardOld` | 新版で置換 |
| 受信済・実行前 | KV `script.current` / `script.status` 照合 | 実行スキップ |
| 実行中 | `kill.{job_id}` subscribe + 子プロセス kill | 強制終了 |

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
- **イベントログ**: backend / agent から `tracing-windows-eventlog` で Windows イベントログにも出力すると、運用チームが普段の Win 監視ツールで気付ける
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

1. Backend が新版バイナリを `agent_releases` Object Store にアップロード
2. KV `agent_config.target_version` を更新
3. 各 Agent は KV を watch、自バージョンと差分検知
4. Object Store からダウンロード → 検証 (署名・ハッシュ)
5. 自己置換 → サービス再起動

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

### Sprint 5 (品質向上)

- [ ] 監視・メトリクス
- [ ] 大規模テスト (シミュレーション 3000 台)
- [ ] バックアップ / 復旧手順
- [ ] ドキュメント整備

### Sprint 6 (HA 化、必要時)

- [ ] NATS 3 ノードクラスタ
- [ ] Backend 冗長化 + LB
- [ ] SQLite → Postgres 移行

---

**END OF SPEC**
