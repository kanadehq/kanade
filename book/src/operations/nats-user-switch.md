# Switching the broker to role-level users

This is the operating procedure for moving the NATS broker from the single
shared token to the three role users (`agent`, `backend`, `breakglass`), with
the revert, and for checking readiness before and fallout after.

> **Nothing here is run by any automation.** No installer, scheduled job,
> backend task or agent performs this procedure or any step of it. The broker
> is switched only by a person running the deployment script on the broker
> host, and the checks below are run by that person. The expected-mode setting
> in the backend only changes what is *reported* as unexpected; it changes how
> no process connects.

## Why it needs a procedure

A broker configuration cannot hold both a token and users. The moment it is
reloaded with users, a client that presents only the token is refused. A host
that is not ready is therefore locked out **at that instant**, and because a
locked-out host also disappears from the broker's connection list and stops
heartbeating, it looks exactly like a powered-off one. Two consequences:

* readiness has to be established **beforehand, from outside the broker**;
* the damage has to be measured **afterwards by comparison** with what was
  alive before.

Both are done with `scripts/ops/nats-switch-check.ps1`, which talks only to
the backend HTTP API with an API token. It never connects to the broker and
never prints a credential.

## Choosing the bcrypt cost

Use **cost 8 with a long random password**, preferably 32 randomly generated
alphanumeric characters, for each role. The hash helpers default to 8 and
still accept an explicit cost. A higher cost adds no practical protection
against guessing a 32-character random password, but consumes broker CPU on
every authentication and can turn a reconnect storm into minutes of downtime.
This recommendation depends on randomness: a memorable or reused password is
not a substitute. Below cost 10, the helpers warn for fewer than 24 characters,
letters only, digits only, or a case-insensitive run of `password`, `qwerty`,
`letmein`, `welcome`, `admin`, or `kanade`. This heuristic does not prove
randomness; no warning is not a strength guarantee. Warnings do not refuse the
input, and the nats CLI's existing minimum-length refusal remains in force.

These measurements came from `crates/kanade-shared/tests/nats_auth_burst.rs`
against a **real nats-server 2.14.3**, using the role-level users configuration
and the real `kanade_shared::nats_client` connect path. The host had an
**AMD EPYC 7551 shared vCPU allocation, 2 vCPU and approximately 1 GB RAM**.
The burst had **N = 50 clients connecting simultaneously**, at broker startup
and again after broker restart. Absolute times belong to that machine;
**re-measure on the real production broker host** with the fleet's load.

| bcrypt cost | One verification, one thread | 50-client startup / restart burst |
|---|---|---|
| 8 | ~25 ms | All 50 authenticated; median 4–5 s, maximum 7.5 s, with authorization timeout 10 s and 30 s |
| 9 | ~95 ms | All 50 authenticated, but median 66–88 s with authorization timeout 30 s; with the shipped 2 s, median 60–78 s and approximately 500–600 `Authentication Timeout` answers |
| 10 | ~190 ms | Practically none authenticated within 90–120 s (0–2 of 50), with the shipped 2 s and with 10 s and 30 s |
| 11 | ~375 ms | None or almost none authenticated (0–1 of 50), with the shipped 2 s |
| 12 | ~720 ms | Burst not measured |

A user-credential connect makes the broker perform **two bcrypt
verifications**: the client's credential probe and the real connection.
Retries repeat that work. `authorization { timeout }` is accepted in both
token and users configurations; the default 2 s was verified to be enforced.
Raising it did not rescue cost 9/10, so the limit is not only the broker's
authorization timeout. Client-side probe/connect timeouts are a suspected
amplifier, still requiring separate investigation. The cost 8 results above
are not a guarantee for the shipped 2 s timeout or for 200 clients.

The probe logs state changes using fixed codes: `nats_probe_accepted`,
`nats_probe_rejected` (broker authorization violation only), and
`nats_probe_inconclusive` (including TLS, proxy, timeout and reachability
failures). Inconclusive attempts do not count as credential refusals. These
diagnostics contain no destination or broker error text, and acceptance alone
does not establish a WebSocket connection's authenticated username.

A collapse looks like `Authentication Timeout` in both client and broker logs,
clients repeatedly retrying, and heartbeats absent for minutes after a broker
restart. Do not interpret that as a successful switch merely because the
broker process is running.

To measure on the real broker host, make `nats-server` available in PATH and
run the same ignored test in release mode:

```sh
KANADE_AUTH_BURST_N=200 KANADE_AUTH_BURST_COSTS=8,9,10 cargo test -p kanade-shared --release --test nats_auth_burst -- --ignored --nocapture
```

Compare startup and restart results with the fleet's reconnect timeouts and
recovery deadlines before switching production.

## Prerequisites

1. **Client versions.** User credentials arrived in **0.61.0** (the release
   whose client library presents a configured user and falls back to the
   token). Agents, the backend and the break-glass CLI all need at least that.
   The script's `-MinAgentVersion` defaults to `0.61.0`. The shipped users
   configuration additionally assumes two later fixes: a connection-liveness
   probe that does not publish to `_INBOX`, and an agent that never overwrites
   object-store objects (so it needs no stream purge right). They are not in
   tag `0.62.1`; use the first release that contains them and pass it as
   `-MinAgentVersion`. With an older agent behind the current users file, a
   host can connect but then hit permission violations.
2. **Credentials are distributed.** Every agent holds the agent role's user
   pair, delivered by the credential distribution jobs (or by the generated
   installer carrying the pair). The check job that reports whether the pair
   is present must have run on every host.
   * The check job is not part of this repository, so the script has no
     default for `-CheckName`. The contract it expects: the check reports
     only whether **both** user and password are present (never a value),
     registers fleet-wide, and reports `ok` only when both are. If the name
     is wrong or the check is not fleet-wide, every host simply shows
     `no-check-result`.
   * `Readiness` must list nothing (step 1 below).
3. **Backend and break-glass hosts** are provisioned by their own deployment
   scripts (`deploy-backend.ps1` / the backend setup script with
   `-NatsUser` / `-NatsPassword`; the break-glass credential by hand on the
   operator machines). These are not distributed to agents and are **not
   covered by the script**: use the manual checklist below.
4. **A tested way to reach the broker host** that does not depend on the
   broker, the backend or the fleet: console, RDP/SSH from a management
   network, whatever you have. Test it *before*; the revert needs it.
5. **The old configuration is kept.** Know where the token and the previous
   installed `nats-server.conf` are. The deployment script's revert restores
   the *shipped* token block only; hand edits made inside that block are not
   restored.
6. **The broker service restarts itself** (Windows service recovery,
   `Restart=on-failure` on Linux), and so do the agents (below).
7. An **API token** for the backend, in `KANADE_API_TOKEN` (or `-Token`), and
   the backend URL in `KANADE_BACKEND_URL` (or `-BackendUrl`). Prefer an
   `https://` URL: the token travels in the request header.
8. **The deployment switch is available in the release you deploy from.** The
   switch and the revert are made by an opt-in mode of the broker deployment
   scripts (Windows and Linux paths), which is separate work from this page.
   Before relying on this procedure, confirm in that release's deployment
   documentation and scripts that a users mode **and** a way back to the
   token both exist, record the release or artifact you will run them from,
   and read their exact usage. If either is missing, stop: there is no
   supported switch or emergency revert to follow.

### Manual checklist: backend and break-glass

Tick every line before the switch; the script cannot see these.

- [ ] The backend host holds the backend user pair and runs a 0.61.0+ backend
      (check the version it reports and its log for a user connection).
- [ ] The backend's own credential is **not** the agent pair and is not in the
      installer settings.
- [ ] The break-glass CLI on each operator machine is 0.61.0+, holds the
      break-glass pair, and has been exercised once against a broker running
      the users configuration (a rehearsal broker is fine).
- [ ] You know which of these hosts is also an agent host. If the backend host
      runs an agent, a backend lock-out also shows as that agent vanishing.

## The order

All commands are run from a checkout of the repository on a machine that can
reach the backend. Use `pwsh` (PowerShell 7) where you can: it lets the
script take the backend's own clock from the response `Date` header. On
Windows PowerShell 5.1 it falls back to the local clock and says so; keep the
clock correct.

### 1. Readiness

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Readiness -CheckName <credential-check> `
    -MinAgentVersion 0.61.0   # or the first release with the two later fixes
```

It lists every registered agent that is **not** ready, with the reason, and
exits 1 if there is any. A host is ready only if all hold:

| reason | meaning |
| --- | --- |
| `agent-version-too-old(..)` / `agent-version-unknown` | the agent is older than the minimum, or reports no parseable version |
| `no-recent-heartbeat` | not online now and not seen within `-RecentWithinHours` (default 24) |
| `no-check-result` | the check job has never reported for this host |
| `check-not-ok(<status>)` | the check reported something other than `ok` |
| `check-stale` | the check's last result is older than the backend's staleness window |
| `check-older-than-CheckedSince` | with `-CheckedSince <UTC date>`, an `ok` from before the distribution does not count |

Continue only when it prints "every registered agent is ready". Exit code 2
means the backend could not be read and **nothing was determined**.

What it cannot know: an `ok` is the agent's own report that a pair is
present, not proof that the values are correct or that the broker will accept
them; hosts that never registered are invisible; the backend and break-glass
are not covered.

The backend API has no per-host "online" field, so the script uses
`last_heartbeat` against the backend's own two-minute alive threshold, and
joins `/api/agents` with `/api/checks` by `pc_id`.

### 2. Snapshot

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Snapshot -SnapshotPath before.json
```

Records the hosts alive **now** (hostname, agent version, last heartbeat,
authenticated NATS user as the backend reports it). Hosts already offline are
deliberately left out so they are never counted as lock-outs. It will not
overwrite an existing file without `-Force`.

### 3. Switch

On the broker host, with the broker deployment's opt-in users mode (prerequisite
8). Use exactly the options its own documentation gives for your release; this
page does not repeat them, because they belong to the deployment scripts and
can differ between releases. Applying the mode must reload or restart the
broker; note whether your path does that itself or leaves it to you.

Write down the **UTC time** at which the broker applied the new configuration;
`Compare` needs it as `-SwitchedAt`. Then set *Settings → Server → Expected
NATS authentication* to `users` so the backend reports anything unexpected.
Its findings are held back for five minutes after that change, which does not
affect the script.

### 4. Compare, after the fleet has had time to reconnect

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Compare -SnapshotPath before.json `
    -SwitchedAt 2026-01-01T09:00:00Z -WaitSeconds <see below> -MaxDisappeared 0
```

A locked-out host still looks alive for up to two minutes after its last
heartbeat, so the script refuses to judge sooner than that plus one heartbeat
interval (`-HeartbeatSeconds`, default 60; set it to your fleet's configured
interval) and exits 3 "too early". `-WaitSeconds` makes it wait before
reading. Run it again after waiting rather than reading a partial answer.

**How long to wait is the larger of two numbers, both counted from the switch
time you wrote down:**

* the script's own floor: 120 s plus the fleet's heartbeat interval; and
* four times the worst reconnect time measured for your clients (below), plus
  the fleet's heartbeat interval.

The second term used to be a guess. An agent that has lost its connection does
not heartbeat again until it has reconnected, and on a reconnect the client's
credential selection first probes the broker with each credential it holds and
then connects for real, so a reconnect is several round trips with their own
timeouts and the client's reconnect backoff in between, not one. On a slow or
busy machine that can add up to tens of seconds. Comparing sooner than that
reads agents that are still reconnecting as disappeared.

#### How the reconnect time was measured

The conformance suite (`nats_role_conformance`, run by the Integration
workflow on Ubuntu, Windows and macOS) times it on every run. It restarts the
broker, or on Linux and macOS reloads it from the token to the users block with
every live connection cut through a proxy, under a running real backend and
agent, and counts from the instant before the restart or reload until each of
these has worked again, all measured side by side from that same instant:

* the agent's next heartbeat, seen by an observer connected straight to the
  broker;
* the backend's ping of the agent returning 200;
* a break-glass `kanade run` completing;
* a fresh call made with the agent role and with the backend role through the
  real `connect` helper (so the helper's probe is inside the number).

Conditions of that measurement, which are not your fleet's: a loopback broker
on a hosted CI runner, three test processes running in parallel on the
machine, the agent heartbeating every second (so the heartbeat adds about one
second, not your interval), and the broker version the workflow pins
(`NATS_SERVER_VERSION`). A real fleet has network latency, a busy broker,
many clients reconnecting at once and a longer heartbeat interval; those make
it slower, not faster. The CI figure is therefore a lower bound on what to
expect and not a recovery guarantee. The right number for your fleet comes
from rehearsing on a few hosts: note when each host's first heartbeat arrives
after a switch of a test broker or one site, and take the worst.

The distribution per operating system (count, minimum, median, 95th
percentile, maximum, and how many waits hit their bound) is written to the
Integration workflow's step summary, and a manual run of the workflow can
repeat the suite (`conformance_repeat`) to collect more samples. Read the
`reconnect` rows and use the worst one on your clients' operating system as
the figure above. If it is tens of seconds even on a loopback runner, plan the
wait in minutes, not in the two or three the old example implied.

It prints:

* **Disappeared**: alive in the snapshot, not alive now. These are the
  lock-out candidates.
* **Heartbeated after the switch and alive now**: indirect evidence of
  reconnection.
* **New hosts**: alive now and not in the snapshot.
* **Live hosts by authenticated NATS user.** Expected: every agent as
  `agent`. Unexpected: `shared-token` (still on the old token),
  `no-auth` (the broker authenticated nobody), `unknown`, another role's
  user, or *absent* (never correlated, which is unknown and not "old
  credential"). Values last changed before the switch are noted, because the
  backend keeps the last value it saw.

The exit code is 1 when more hosts disappeared than `-MaxDisappeared`. If no
host from the snapshot is alive it says the backend itself is probably locked
out; revert first and look at the backend host.

If the backend cannot be reached it retries (`-Retries`, `-RetryDelaySeconds`)
telling you so, because it may be reconnecting to the broker. If it still
fails it exits 2 saying plainly that this is **not** an empty result.

### 5. Decide

* Disappeared count within what you accepted, and the user table shows only
  `agent`: keep the switch and go to *Afterwards*.
* A handful disappeared: decide per host with the next section while the
  rest of the fleet runs; reverting for a few hosts is usually wrong.
* Many disappeared, a non-`agent` user on live hosts, or the backend looks
  locked out: **revert** (below), re-run `Compare` with the same snapshot to
  confirm the fleet is back, and find out why before trying again.

## Revert

On the broker host (through the way you tested in the prerequisites): use the
same deployment path's token mode to restore the token configuration, then
reload or restart the broker. Know the exact revert command **before** the
switch (prerequisite 8) and keep it where you can reach it without the fleet.
Hand edits made inside the shipped token block are not restored by it.

Set the expected authentication mode back to `token`. Confirm client recovery
and service-manager restart settings as described in the next section.
Confirm with:

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Compare -SnapshotPath before.json `
    -SwitchedAt <time of the revert, UTC> -WaitSeconds <as in step 4> -MaxDisappeared 0
```

## Recovery deadlines and the production switch gate

Do not switch production until the release containing the recovery watchdog
has passed a manual Integration dispatch with `conformance_repeat=20` and
zero failures on Ubuntu, Windows and macOS. The previous release's promises
were not sufficient: a process could stay alive without heartbeats. The
injected-fault tests and the repeated conformance results must both pass.

The client recovery targets, counted from when the broker becomes reachable
and accepts the newly provisioned credential, are:

* resume communication within **30 s** (`RESUME_BOUND`); or
* exit non-zero within **195 s** (`EXIT_BOUND`) so supervision can restart it.

These recovery bounds assume the broker can authenticate the fleet's
reconnects within the clients' own timeouts. A high bcrypt cost can break
that assumption; see [Choosing the bcrypt cost](#choosing-the-bcrypt-cost).

These timers are defined in `crates/kanade-shared/src/nats_client.rs` and all
derive from the protocol PING interval, set explicitly to 60 s (async-nats'
default). A reconnect attempt allows the 4 s backoff, 3 s probe plus 1 s
guard, and 5 s handshake; 30 s allows two attempts plus slack. An
inconclusive probe skips the attempt instead of guessing from the previous
broker mode.

The watchdog observes each connection's own receive progress. Idle clients
send a protocol PING every 60 s and the broker's PONG is the progress, so
liveness costs no traffic beyond the library's default and needs no subject
permission or application message. A flush alone proves only a local write.
Every 5 s the watchdog runs a purely local check (counters and a queued
flush); it makes no network connection while receive progress is recent. Only
after **180 s** (three ping intervals, two full intervals of margin) without
receive progress does it open a fresh credential witness. If the witness
connects or is explicitly refused while the original connection still has no
progress, the process logs the failure and exits. The exit budget adds the
5 s check interval, 5 s flush timeout and 5 s witness timeout to the 180 s
stall threshold: 195 s, inside the 5-minute target. Supervision starts
immediately after client creation, before subscriptions or resource bootstrap
can block; fatal failure bypasses application shutdown so it cannot extend
this deadline.

After a broker outage the watchdog first lets a client that is still in its
reconnect backoff try again. A witness that cannot reach the broker never
causes an exit and is repeated at most every 30 s; the first witness that
reaches it again gives the client a further 30 s to resume, and any received
progress clears the outage state, so a later genuine stall is judged against
the normal 180 s threshold. The expected wait for a client that is merely
reconnecting is therefore its reconnect backoff (up to 4 s) plus the handshake,
well inside the 30 s resume bound; a silently stranded client costs up to
195 s before its restart.

A broker that is unavailable, or whose mode cannot be determined by the
probe, is waited for indefinitely. The deadlines assume the client can reach
and authenticate to the broker; they are not a network outage deadline.
Repeated actual credential refusals still fail visibly. A single refusal is
retried by the pinned connection library and does not trigger an immediate
exit. Restart coverage applies on all three operating systems; signal-based
reload coverage is provided by conformance on Linux and macOS only.

Service-manager delay is additional: Windows recovery actions wait 5 s, then
15 s, then up to 60 s; systemd uses `RestartSec=5`; launchd uses
`ThrottleInterval=10`. Thus the slowest configured restart after a watchdog
exit is 255 s from broker availability on Windows, 200 s on systemd, and 205 s
on launchd, before application startup and the next heartbeat. Start-rate
limits or disabled recovery can prevent a restart; check them before the
switch. The CLI is not a service: its caller must rerun it after non-zero exit.

### Expected logs

During a restart, expect `event: disconnected`, IO errors and possibly
`expected INFO, got nothing`. A refused user probe during token mode is
expected. `NATS credential selected` names the selected shape (`user` or
`token`) without exposing credentials. An inconclusive probe logs
`NATS credential probe inconclusive; deferring this attempt` at debug level.

On recovery, expect `event: connected` and resumed heartbeats. If the
original client stalls while a witness reaches the broker, expect
`NATS client made no receive progress while a fresh credential witness
reached the broker; exiting for a supervised restart`, followed by a non-zero process
exit and the service manager's restart. Persistent wrong credentials log
`the NATS broker keeps refusing this role's credential`; a terminated task
logs `NATS connection task has terminated`. A live, silent process after the
deadline is a defect requiring investigation, not an expected switch state.

## Credential badges on WebSocket connections

The Agents table distinguishes a named identity from `unknown` (the
credential name is unproven), `auth-pending` (observed before authentication
acceptance within the broker deadline), and `auth-unproven` (acceptance itself
cannot be established). None of these uncertainty states is an authentication
failure, and none changes command-signing status. The acceptance sentinels
must never appear as green named-user badges.

A successful user/password handshake through the actual WSS reverse-proxy URL
establishes acceptance of that probe. It does not prove another connection's
identity or the complete listener policy. WebSocket token, `no_auth_user` and
cookie overrides are independent of the backend's TCP connection. The badge
therefore remains unproven when listener-specific identity evidence is absent;
do not reinstall an agent, rotate credentials or change broker settings just
to change that badge.

Even validating the effective configuration and matching its digest is
insufficient to treat arbitrary `authorized_user` values as authenticated
identities. On the tested broker, a users-authenticated WebSocket client can
also supply an ignored `nkey` CONNECT field. `/connz` reports that field in
preference to the username actually checked, including another configured
username (see the upstream
[`getRawAuthUser`](https://github.com/nats-io/nats-server/blob/v2.14.1/server/client.go)
and [user/password validation](https://github.com/nats-io/nats-server/blob/v2.14.1/server/auth.go)). The real-broker WSS regression test authenticates as one user while
reporting another, then checks that SQLite, credential health and evidence
history retain only the safe `unknown` classification. Negative handshakes
are checked for broker authorization errors through the same TLS proxy; TLS
verification uses a disposable trusted certificate and is never disabled.

The WSS override regression also exercises anonymous `no_auth_user`, the
broker's `user_cookie`/`pass_cookie` and `token_cookie` paths, and broker
rejection of missing or incorrect cookies. The WSS lifecycle regression
changes a password with a real broker reload, rejects the old password,
checks the replacement connection id, and restarts the broker behind the
same proxy. Old broker identities and expired credential-health observations
remain unusable as identity evidence. These cases run in the existing
integration CI job alongside the other `live_connz_` tests.

### broker が生成する identity proof（v2）

WSS 接続の名前には broker が**認証に成功した結果**を使う。CONNECT の
nkey/name、旧 authorized_user、期待する認証モード、名前の allowlist、
probe の成否を認証結果の代用にはしない。producer は
tools/nats-identity/producer.patch と auth_identity.go にあり、
build.py の SHA-256 で固定した nats-server 2.14.1 のソースに適用する。
クライアントの wire protocol、認証判断、権限、JetStream、Caddy は変更しない。

v2 は認証結果と一緒に取得する情報の重複を除いた契約である。v1 の
接続ごとの server id/start、cid/start/transport のコピーを廃止し、
既存の監視応答の外側で共有する。情報の結び付けは次のとおり。

| 場所 | 結び付けと検証 |
| --- | --- |
| /varz | server_id、start、auth_identity_version=2、auth_identity_generation。同じ epoch が監視 walk の前後で一致すること |
| /connz の共通部分 | server_id と auth_identity_server_start が /varz の id/start に一致すること。時刻・ページ境界も既存の検査を通す |
| 各 connections 要素 | broker 発行の cid、start、type。認証結果と同じ既存の接続ロック内でコピーし、cid が非ゼロ、start が broker 開始以降・現在以前、type が nats/websocket であること |
| 各要素の auth_identity | version=2、generation、sequence、state、mechanism、source、任意の public_user。accepted の generation は /varz と一致し、sequence は非ゼロであること |

public_user は認証で検証されたサーバー側の username のみである。
構成内の全 secret との平文・bcrypt 衝突検査を通し、ASCII の英数字と
._- のみ、最大64文字、予約ラベル以外に制限する。User オブジェクト、
パスワード、token、JWT、鍵、CONNECT の任意の主張、peer address は
この metadata に含めない。cookie 経由でも認証結果を使い、source=cookie
で区別する。token は shared-token、anonymous/no_auth_user は no-auth、
NKey/JWT/TLS mapping/delegated/custom は auth-accepted-unnameable とし、
pending/validated、rejected、stale を名前に変換しない。

設定の検査は設定構築時だけ行う。今回の最適化は、同じ設定内の同一 secret
を重複除去し、各bcryptハッシュを一度解析し、平文衝突を先に除外する。
ハッシュ比較の結果や secret をグローバルにキャッシュしない。
認証時には小さな結果を保存し、監視時には既存の接続ロックで値をコピーする。
v2 は ConnInfo に値を直接保持するため、監視時の接続ごとの別ヒープ
割り当てを減らせる。PUB/SUB の通常配送経路には処理を追加しない。
設定・認証・監視が無費用になるわけではない。

観測時刻と broker 時計との差は60秒以内に制限する。名前と90秒の期限は
同じ選択済み接続に由来し、失効処理とともに同じトランザクションで保存する。
Agents API は期限を読取時にも検査する。poll 失敗時には名前を失効させる。
認証待ちの新しい cid が併存しても、選択した認証済み接続の期限が欠けない。

#### 配布・更新・rollback

これは上流 NATS に含まれる機能ではなく、固定ソースへの fork patch である。
operator がビルド・配布・更新保守するか、上流への採用が必要である。
新しい Kanade と producer を同じ v2 契約で配布する。v1 は未サポート扱いで、
互換 shim で名前を回復しない。未改造・旧版・不明版・metadata 欠落・不整合は
unknown/auth-unproven に留める。producer を未改造版に戻せば次のpollで
名前が失効する。既存バイナリの置換には broker の再起動が必要で、すべての
接続が再接続する。新 epoch は再認証まで未証明である。reload でも
generation を更新し、古い認証結果を再利用しない。権限や認証方式を緩めて
表示を緑にする運用はしない。

検証は Python 3.11 以上（native package fixture の tomllib）、Go 1.27.2
（.tool-versions と integration.yml）、Bun、Python TLS proxy、OpenSSL が必要。
グローバル設定を変更せず、対応する Python を子プロセスにも選ぶこと
（docs/local-verification.md を参照）。python3 tools/nats-identity/verify.py は
固定ソースから producer をビルドし、Go race テスト、実 broker の
WSS/TCP と producer 由来の Agents browser テストを実行する。
2件のpositiveテストと必須browserテストの実行を検査し、空振りは成功にしない。
本作業で本番の設定変更、再起動、配布、採用判断は行っていない。

#### 初回測定（最適化前、保持した結果）

初回は revision d0d12836 の結果を measurements-initial.json に内容を変えず
保持した。Go 1.27.2、Darwin arm64、8 CPU の同一ホストで各負荷5サンプル。
default は GOMAXPROCS=4/GOMEMLIMIT=off、constrained は
GOMAXPROCS=1/GOMEMLIMIT=768MiB。中央値は以下のとおり。初回は
variantごとに5回まとめて実行しており、後述の交互順序とは異なる。

| profile | 負荷 | baseline ms/op | patched ms/op | 時間差 | B/op（baseline → patched） | allocs/op（baseline → patched） |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| default | TCP・平文password churn | 0.482 | 0.637 | +32.08% | 62888 → 64182 | 179 → 179 |
| default | connz・128接続 | 0.505 | 0.787 | +55.74% | 132171 → 204187 | 395 → 525 |
| default | 監視付き64B request/reply | 0.181 | 0.195 | +7.89% | 1318 → 1336 | 21 → 21 |
| default | cold config・同一bcryptハッシュ×3 | 0.328 | 412.581 | +125816.99% | 129349 → 182872 | 445 → 856 |
| constrained | TCP・平文password churn | 0.532 | 0.488 | -8.25% | 62946 → 64220 | 180 → 180 |
| constrained | connz・128接続 | 0.542 | 0.836 | +54.30% | 131930 → 204016 | 394 → 523 |
| constrained | 監視付き64B request/reply | 0.171 | 0.174 | +1.74% | 1318 → 1325 | 21 → 21 |
| constrained | cold config・同一bcryptハッシュ×3 | 0.343 | 415.702 | +121045.97% | 129354 → 182816 | 445 → 854 |

特にdefaultのTCP churnは **482047 → 636671 ns/op（+32.08%）**。
5サンプルの範囲もbaseline 468165–538897、patched 580087–712285 ns/opで
重ならない。これをnoiseとは扱わない。constrainedの中央値
531838 → 487958 ns/op（−8.25%）とは別の結果である。
defaultのconnz応答は46503 → 91203 byte、約96%増だった。
3ユーザーのconfig構築は327661 → 412580854 ns/opであり、
初回の大きな設定費用を省略しない。このfixtureは3人が**同一**の
cost-8 bcryptハッシュを共有していた。WSS接続費用や独立ハッシュの費用は
この初回測定からは分からない。client_bytesはResetTimerより前に報告したため
初回出力には残っておらず、初回のchurn B/opを保持メモリと解釈してはいけない。

#### 最適化後の比較

measurements.json は各profile/variant/6負荷を5回ずつ記録し、生のGo出力、
各sample、中央値、差分、固定ソース・patch・producer・benchmarkのSHA-256、
初回結果のSHA-256を含む。両variantには同一benchmarkコードをコピーし、
初回と同じNATS 2.14.1/Go 1.27.2、500ms、profile設定を使った。
各roundは別のgo testプロセスでcount=1を実行し、第1/3/5回を
baseline→patched、第2/4回をpatched→baselineにして順序を交互にした。
元の4負荷を保持し、client_bytesをResetTimerの後に報告する修正を加え、
独立ハッシュのcold configとWSS負荷を追加した。

| profile | 負荷 | baseline ms/op | patched ms/op | 時間差 | B/op（baseline → patched） | allocs/op（baseline → patched） |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| default | TCP・平文password churn | 0.544 | 0.633 | +16.27% | 62944 → 64311 | 180 → 180 |
| default | connz・128接続 | 0.538 | 0.667 | +23.88% | 132221 → 156984 | 395 → 397 |
| default | 監視付き64B request/reply | 0.219 | 0.213 | -2.52% | 1332 → 1339 | 21 → 21 |
| default | cold config・同一bcryptハッシュ×3 | 0.371 | 129.470 | +34777.44% | 129360 → 149536 | 445 → 751 |
| default | cold config・独立salt/異なるpassword×3 | 0.363 | 371.106 | +102156.15% | 129345 → 181064 | 445 → 827 |
| default | WSS・bcrypt cost 8 churn | 51.791 | 50.636 | -2.23% | 224336 → 214332 | 1143 → 1138 |
| constrained | TCP・平文password churn | 0.489 | 0.508 | +3.96% | 62958 → 64280 | 180 → 180 |
| constrained | connz・128接続 | 0.543 | 0.654 | +20.42% | 131949 → 156689 | 395 → 396 |
| constrained | 監視付き64B request/reply | 0.178 | 0.178 | +0.32% | 1327 → 1332 | 21 → 21 |
| constrained | cold config・同一bcryptハッシュ×3 | 0.351 | 125.686 | +35661.01% | 129354 → 149508 | 445 → 750 |
| constrained | cold config・独立salt/異なるpassword×3 | 0.354 | 391.807 | +110590.34% | 129365 → 181064 | 445 → 826 |
| constrained | WSS・bcrypt cost 8 churn | 50.973 | 42.394 | -16.83% | 213481 → 214514 | 1141 → 1138 |

同一ハッシュのconfig構築は初回patched約413/416 msから約129/126 msへ
短縮した。ただし、これは重複比較9回を3回に減らせたケースである。
独立salt・異なるpasswordの3ハッシュでは3候補×3ハッシュの衝突検査が必要で、
約371/392 msが残る。baselineの約0.36 msとは依然として大きな差がある。
検査を省略したりbcrypt costを下げたりしない限り、このCPU費用は残る。
credentialの生成はtimed regionの外だが、NewServerとpolicyの構築は
**毎op冷状態**で行い、warm cache hitへ置き換えていない。
64ユーザー超、cost 12超、distinct secret 256超、重み付き比較仕事量4096超では
名前を出さない安全側の上限も維持している。

defaultのpatched TCP churnは初回0.637 msから今回0.633 msとほぼ同じであり、
この経路の回帰が解消したとはいえない。今回のbaselineも0.544 msに変わって
おり、今回の差は+16.27%。constrainedは+3.96%。今回の範囲は重なるが、
初回の非重複範囲と+32.08%を取り消す根拠にはしない。
認証snapshotの保存や同期の費用は残っている。

connzはdefaultでbaseline比時間+23.88%、B/op +18.73%、応答
46508 → 65264 byte（約40.3%、1接続あたり約147 byte増）。
constrainedでは時間+20.42%、応答46508 → 65229 byte。
defaultのpatched同士では初回からB/op約23%減、allocs/opは525→397、
応答は約28%減となった。ただし初回との時間比較には順序・ホスト状態の差もある。
response_bytesは各sample末尾のserialized応答長を報告し、その5回の中央値である。
v2のcoherentな共有フィールドと値コピーは冗長性を減らすが、追加の監視コストは残る。
この比較はauth=1であり、auth=0でもConnInfoの値fieldのため構造体の拡大はある。
auth=0の時間・allocationは今回測定していない。

保持メモリは両profileでclient構造体6872 → 6952 byte、増分80 byte。
identity_state_bytesも80 byteである。これはgeneration/sequenceと4つの
string headerで、state/mechanism/sourceは固定文字列、publicUserは既存の
immutableなcanonical名の参照を再利用する。新しい文字列のコピーや接続ごとの
secret構造体は作らない。参照するpublic名の長さは最大64 byteで、設定内で
共有される。client_bytesは構造体サイズの実測であり、allocatorのsize class、
stack、socket、TLS、既存の設定の寿命やRSS全体を含まない。
churnのB/opは一時割り当てを含み、80 byteの保持増分とは異なる。
policyと一時的なsecret収集は設定単位であり、新たなidentity履歴は保存しない。
producer・consumerとも名前は最大64 byteで検査する。定数文字列の本体は共有され、
接続あたり新たに確保する動的なidentity文字列のバッファはない。ただし参照先の
canonical名の寿命は接続まで延び得るため、構造体サイズだけでRSS増分は断定しない。
監視用ConnInfoにはv2の値fieldが加わる（64-bit型定義からの計算では88 byte）。
既存のclosed ringもこの空fieldを持つため、MaxClosedClientsの既定10000件なら
field自体で880000 byte相当が増える。閉じた接続の新metadataは保存しないが、
値field化によるこの容量のtradeoffは残る。allocator込みのRSSは未測定である。

監視付きrequest/replyは10msごとにconnzを生成し、64 byteを1往復ずつ送る。
今回の中央値からの逐次処理率はdefaultで約4572 → 4690 req/s、
constrainedで約5632 → 5614 req/s。allocs/opはいずれも21のまま。
これを並列PUB/SUBの最大throughputやtail latencyの保証にはしない。

追加WSS負荷は、一時的なP-256証明書を検証する新しいTLS/WebSocket接続で、
cost-8 bcryptユーザー1人のCONNECTとflushを毎op実行する。
証明書・hashの生成は計時外、接続・TLS・bcrypt認証は計時内。
TLSはbrokerで直接終端し、Caddyや遠隔ネットワークを再現していない。
defaultは約51.8 → 50.6 ms、constrainedは約51.0 → 42.4 msだが、
サンプル範囲が広く、producerで高速化できたという結論にはしない。
この遅い合成負荷の小さい相対差で、元のTCP churnの回帰を相殺しない。
実TLS proxyを経由するpositive/negative試験は別途verifyで維持している。

これらは単一Darwinホスト上のmicrobenchmarkで、GOMEMLIMITはGoのsoft heap
limitにすぎない。**1 GiB Linux VM・swapなしでの試験ではない**。
TLS clientとbrokerは同じプロセス内で、長期RSS、接続storm、JetStream負荷、
p95/p99、本番Caddyや本番のbcrypt costを評価したものではない。
個々の範囲と全sampleを保存し、好都合な値だけを選んでいない。
設定時の数百ms、監視・churnの残存増分は運用上のtradeoffであり、
本番採用を許容したという意味ではない。採用・配布・本番での再起動は、
この実装と測定結果を見た所有者の別判断が必要である。

## Telling a locked-out host from an offline one

From the backend alone you cannot (the script says so every time). Use
evidence outside the broker:

1. **Was it alive just before?** It is in the snapshot, with a last heartbeat
   seconds before the switch: consistent with either lock-out or power loss,
   but a host that had been dark for hours before is not a lock-out.
2. **What did the backend last record for it?** `shared-token` as its last
   NATS user means it was on the old token and never reported the new user.
   A missing user means never correlated, not "old credential".
3. **Reach the machine without the broker**: ping or power state from your
   management tooling, then the agent service state and its log. A lock-out
   shows a running (or restart-looping) agent and authorization errors in the
   log; a powered-off host shows nothing.
4. **Broker side**, on the broker host: the server log shows authorization
   violations for refused connections.

## Dotted pc_ids

A pc_id is the agent's `[agent] id`, and on Linux and macOS that is often an OS
hostname with dots (`m1air.local`, `host.example.com`). Each dot makes another
subject token, so the permission lists in the users configuration cannot use a
single `*` where the pc_id goes. A pc_id of **up to 4 dot-separated labels**
is supported (`host.sub.example.com` is the longest shape intended); the lists
spell out one pattern per label count wherever a wildcard cannot express "one
or more tokens".

A pc_id with 5 or more labels is refused up front, on the token configuration
too: the agent exits at startup with an error naming the limit, `kanade run`
and `kanade agent logs` fail before sending anything, and the backend API
answers `400`. Under the users configuration such an agent would otherwise
connect and then lose every publish.

**Before the switch**, list the registered pc_ids and look for any with 5 or
more labels (that is, 4 or more dots). Give those hosts a shorter `[agent] id`
first; an existing deployment that has one will not start the agent after the
upgrade that adds this check.

**How a lock-out looks** if a host slips through (for example an agent built
before the limit existed): the agent connects as `agent`, so the backend shows
it with the new user, but nothing it publishes arrives. The host goes silent in
the SPA, and the broker log shows lines such as

```text
Publish Violation - Subject "heartbeat.m1air.local"
```

(logged against the `agent` user), and likewise for `host_perf.`, `obs.` and
the other subjects that carry the pc_id. Fix the id and restart the agent; it
needs no broker change.

## Hosts that stay out

Use the same evidence path. A host that is powered off returns when someone
powers it on and needs nothing. A host that is running but refused has no
working user pair or an agent that predates user credentials: put the pair on
it (the deployment script with `-NatsUser` / `-NatsPassword`) or upgrade it,
and restart the agent service; it then connects as soon as the credential is
present. If you cannot fix it by hand soon, and the host matters, revert,
fix the stragglers, and run Readiness again; do not leave a fleet of
unreachable machines in place hoping they recover.

## New machines during the window

A machine installed while the broker runs the users configuration needs the
agent role's user pair at install time, or it cannot connect. Use an
installer generated with the agent pair set in the backend's installer
settings, and still carrying the token (a revert needs it). Such a host is not
in the snapshot; it shows up under *New hosts* and in the user counts. A host
installed with the token only is refused and is invisible to the backend, so
it cannot be seen by any of this; after a successful switch, check that no
installer in circulation lacks the pair.

## Afterwards

1. Confirm connections are all the expected role users: the user table from a
   fresh `Compare` shows only `agent` for agents, and the backend's
   *Expected NATS authentication* findings are empty.
2. Only then plan the removal of the shared token from hosts and from the
   installer settings. That is a **separate, later step**: while the token is
   still stored, a revert still works. Do not remove it in the same window.

## Rehearsal

Run the whole sequence once against a throwaway broker before the first real
switch. Nothing in the rehearsal touches production.

1. **Throwaway broker.** Start a separate `nats-server` on its own port and
   store directory (the development broker uses port 4223), first with the
   shared token, using a configuration copied from the shipped one. Prepare
   the users configuration from the shipped template with bcrypt hashes of
   throwaway passwords.
2. **Backend** pointed at that broker, with its own data directory and API
   token, and **a few agents** with distinct ids, in these states:
   * normal: user pair and token, current version;
   * pair not distributed: the token only (on purpose);
   * old version: the oldest agent you have;
   * stopped: a registered agent that is not running.
3. **Readiness** with a check job that reports for them (or, with none,
   expect every host to be `no-check-result`; rehearse the other reasons and
   the exit code anyway). Confirm it names the undistributed, old and stopped
   hosts and exits 1. Fix them (or accept them) until the list is empty or
   known.
4. **Snapshot**, then **switch** the throwaway broker with a reload signal
   (not a restart), noting the UTC time.
5. **Compare** too early (expect exit 3), then after the wait. Confirm the
   deliberately unprepared agent shows as disappeared, the normal ones show
   as `agent`, and `-MaxDisappeared` above and below the real count gives
   exit 0 and 1.
6. **Reproduce a lock-out** and walk the "locked-out vs offline" section on
   it, including finding the authorization errors in the broker log.
7. **Pause the backend API** briefly while running `Compare`, to see the retry
   messages and the exit 2 when it stays down.
8. **Revert** to the token, restart the unprepared agent if its service
   manager does not, and `Compare` against the same snapshot to confirm the
   fleet is back.

The conformance test (`nats_role_conformance`, see the README) covers the
broker-side behaviour of this switch with live processes; the rehearsal
covers *your* hosts, tooling and access path, which is what goes wrong in
practice.
