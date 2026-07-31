/**
 * Deterministic fake fleet backing the demo API (`cargo make demo`).
 *
 * Everything here is invented — the hostnames, the people, the
 * departments. That is the point: the demo stack exists so screenshots
 * and walkthroughs can be produced without pointing a browser at a real
 * fleet, which would leak internal hostnames and sign-in accounts into
 * whatever the screenshot ends up in.
 *
 * The generator is seeded, so the same PC keeps the same hostname,
 * owner, CPU load and agent version across restarts. Screenshots taken
 * a week apart stay comparable, and a bug reproduced against the demo
 * data reproduces again.
 *
 * Absolute timestamps are NOT baked in here: the fleet carries
 * "milliseconds ago" offsets, and the server converts them against the
 * wall clock per request. Otherwise every relative label in the SPA
 * ("3m ago", "in 2h") would drift into nonsense the longer the demo
 * server stayed up.
 */

/** mulberry32 — small, fast, good enough for fixture data. */
export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const pick = <T,>(r: () => number, xs: readonly T[]): T =>
  xs[Math.floor(r() * xs.length)]!;
const int = (r: () => number, min: number, max: number): number =>
  Math.floor(r() * (max - min + 1)) + min;

// ---------------------------------------------------------------- fleet

/**
 * Hostname prefix for the invented fleet — `KANADE-PC-0001` and so on.
 *
 * Was `ACME`, the Looney Tunes placeholder that western technical docs
 * adopted for "some generic company". It carries no meaning for a
 * Japanese reader, and a screenshot is read, not explained.
 */
export const ORG = 'KANADE';
export const FLEET_SIZE = 248;
/** Newest build — the one most of the demo fleet has already adopted. */
export const CURRENT_VERSION = '1.0.0';

export const DEPARTMENTS = [
  '営業部',
  '開発部',
  '総務部',
  '人事部',
  '経理部',
  '情報システム部',
  'カスタマーサポート',
  'マーケティング部',
] as const;

// Weighted, not uniform: a uniform draw across five sites puts ~20% in
// each, which renders as a head-office branch that is the SAME SIZE as a
// regional sales office — or, worse, smaller than one. The pie chart is
// on the dashboard, so that shape is visible at a glance and reads as
// invented. Weights are roughly HQ-heavy with a tail of small offices.
export const SITES = [
  ['東京本社', 44],
  ['大阪支社', 20],
  ['名古屋支社', 15],
  ['福岡営業所', 12],
  ['札幌営業所', 9],
] as const;

/** Pick from `[value, weight]` pairs. Weights need not sum to 100. */
function weighted<T>(r: () => number, entries: ReadonlyArray<readonly [T, number]>): T {
  const total = entries.reduce((s, [, w]) => s + w, 0);
  let x = r() * total;
  for (const [value, w] of entries) {
    x -= w;
    if (x <= 0) return value;
  }
  return entries[entries.length - 1]![0];
}

const SURNAMES = [
  '佐藤', '鈴木', '高橋', '田中', '伊藤',
  '渡辺', '山本', '中村', '小林', '加藤',
  '吉田', '山田', '松本', '井上', '木村',
] as const;
const GIVEN = [
  '太郎', '花子', '健一', '美咲', '翔',
  '優子', '大輔', '彩', '直樹', '沙織',
  '涼', '真由', '拓也', '恵', '悠斗',
] as const;
const ROMAJI_SURNAME: Record<string, string> = {
  佐藤: 'sato', 鈴木: 'suzuki', 高橋: 'takahashi', 田中: 'tanaka', 伊藤: 'ito',
  渡辺: 'watanabe', 山本: 'yamamoto', 中村: 'nakamura', 小林: 'kobayashi', 加藤: 'kato',
  吉田: 'yoshida', 山田: 'yamada', 松本: 'matsumoto', 井上: 'inoue', 木村: 'kimura',
};

const MODELS = [
  'ThinkPad X1 Carbon Gen 11',
  'ThinkPad L14 Gen 4',
  'Latitude 5450',
  'OptiPlex 7010',
  'EliteBook 840 G10',
  'ProDesk 400 G9',
  'VAIO SX14',
] as const;

/**
 * The OS mix, weighted and dated against the real Windows servicing
 * calendar rather than picked for variety.
 *
 * The point of the `os_eol` check is "act before this bites", so the
 * fleet has to be sitting where a real fleet sits *now*: mostly current,
 * a SMALL minority on the release that expires next, a couple of
 * stragglers already past. The expiring group is deliberately in the
 * teens rather than the dozens — a list an operator could work through
 * this month reads as a fleet under control that found something, which
 * is the product's pitch; eighty of them reads as a backlog. Filling it with Windows 10 22H2 (expired
 * 2025-10) would say the customer ignored a deadline that passed a year
 * ago — a different and much less flattering story than the one the
 * check is for, and not one an operator evaluating the product wants to
 * see themselves in.
 *
 * Servicing dates (Home/Pro):
 *   25H2 → 2027-10-12   current
 *   24H2 → 2026-10-13   expires in ~2 months ⇒ the warning
 *   23H2 → 2025-11-11   already past ⇒ the failure
 */
const OS_BUILDS = [
  { caption: 'Windows 11 Pro', version: '25H2', weight: 88, build: '26200.6584' },
  { caption: 'Windows 11 Pro', version: '24H2', weight: 4, build: '26100.4946' },
  { caption: 'Windows 11 Pro', version: '23H2', weight: 2, build: '22631.5909' },
] as const;

/** Servicing end per `version`, for the `os_eol` check's detail line. */
export const OS_EOL_DATE: Record<string, string> = {
  '25H2': '2027-10-12',
  '24H2': '2026-10-13',
  '23H2': '2025-11-11',
};

// ------------------------------------------------- command signing (#1165)
//
// DECLARED ABOVE `build()` ON PURPOSE. `build()` runs at module init and
// reads the consts below — and `const` is NOT hoisted, so putting this at
// the end of the file (where it reads most naturally) crashed the mock on
// boot with `Cannot access 'BACKEND_SIGNING_KEY' before initialization`.
// `tsc --noEmit` passes either way: this is initialisation ORDER, which the
// type checker does not model, so the ordering has to be deliberate.

/** The backend's own signing key, as `GET /api/command-signing` reports it. */
export const BACKEND_SIGNING_KEY = {
  kid: 'kanade-2026-07',
  fingerprint: 'b7c41f9a20e6d358',
};

/** The key it rotated away from. Hosts keep it so in-flight commands signed
 *  with the old key still verify — a ring, not a single key. */
const PREVIOUS_KID = 'kanade-2026-01';
const PREVIOUS_FP = '3d9e0a17c4b85f62';

/**
 * Every host: provisioned with the current ring, and enforcing.
 *
 * An earlier draft spread the fleet across all seven states the badge can
 * show, on the theory that a demo should exercise the feature. That was the
 * wrong theory. Those states describe a fleet MID-ROLLOUT — `wrongKey`,
 * `staleRing`, `none` are migration debris — and the steady state, which is
 * what a prospective customer is being shown, is simply: every host verifies
 * who issued a command, and refuses one it cannot verify.
 *
 * A screenshot of a half-migrated fleet does not advertise the feature, it
 * advertises the migration. Uniform here is not laziness; it is the claim.
 */
const RING = [
  `${BACKEND_SIGNING_KEY.kid}:${BACKEND_SIGNING_KEY.fingerprint}`,
  `${PREVIOUS_KID}:${PREVIOUS_FP}`,
];

export type DemoPc = {
  pc_id: string;
  /** Command-signing ring this host trusts (#1165). `undefined` means the
   *  agent predates the field — a distinct state from holding none. */
  command_keys: string[];
  /** Whether it refuses unverified commands (#1250). `undefined` = the agent
   *  cannot say, which is not the same as `false`. */
  enforcing: boolean;
  hostname: string;
  os_family: 'windows' | 'linux';
  agent_version: string;
  /** Heartbeat inside the 2-minute liveness window. */
  online: boolean;
  heartbeat_ms_ago: number;
  cpu_pct: number;
  mem_used_bytes: number;
  mem_total_bytes: number;
  disk_total_bytes: number;
  disk_free_bytes: number;
  agent_cpu_pct: number;
  agent_rss_bytes: number;
  last_logon_user: string;
  last_logon_display_name: string;
  email: string;
  dept: (typeof DEPARTMENTS)[number];
  site: (typeof SITES)[number][0];
  model: string;
  serial: string;
  os_caption: string;
  os_version: string;
  os_build: string;
  groups: string[];
};

const SERVER_MODELS = ['PowerEdge R450', 'ThinkSystem ST250 V2', 'PRIMERGY TX1330 M5'] as const;

function build(): DemoPc[] {
  const r = rng(0x6b616e61); // "kana"
  const out: DemoPc[] = [];
  // Numbered per kind, so each series stays contiguous instead of showing
  // gaps wherever a machine of another kind was pulled out of the sequence.
  let pcSeq = 0;
  let svSeq = 0;
  let mtSeq = 0;

  for (let i = 1; i <= FLEET_SIZE; i++) {
    // Three kinds of machine, and the hostname says which — `-SV-` for the
    // always-on boxes, `-MT-` for the terminals IT works from, `-PC-` for
    // everyone else. Worth encoding in the id rather than only in group
    // membership: an audit payload, a job result or a filter box shows the
    // hostname and nothing else, and "is this a server?" is the first
    // question anyone asks of an unfamiliar host.
    //
    // Fixed strides, not PRNG draws, so the ids stay put across restarts —
    // a demo whose hostnames move between runs cannot be screenshotted
    // twice. Mutually exclusive so no machine is both.
    const isServer = i % 83 === 9;
    const isOpsTerminal = !isServer && i % 71 === 6;
    // A believable fleet is mostly-current with a visible tail still
    // catching up — a 100%-adopted rollout makes the Rollout page look
    // like it does nothing.
    const version =
      i <= 198 ? CURRENT_VERSION : i <= 236 ? '0.9.6' : '0.9.3';

    // 17 offline hosts out of 248: laptops that went home, not an outage.
    // Servers are exempt — "常時稼働させている機器" that shows as offline in
    // the same screenshot contradicts its own group description.
    const online = isServer || i % 15 !== 3;
    const surname = pick(r, SURNAMES);
    const given = pick(r, GIVEN);
    const romaji = ROMAJI_SURNAME[surname] ?? 'user';
    // Suffixed once, then reused, so the account and the address cannot
    // drift apart again.
    const account = `${romaji}${int(r, 1, 99)}`;
    const dept = pick(r, DEPARTMENTS);
    const site = weighted(r, SITES);
    const os = weighted(r, OS_BUILDS.map((o) => [o, o.weight] as const));
    // A handful of Linux boxes — kiosk / build hosts. Keeps the
    // os_family column from being a single value everywhere.
    const linux = i > 240;
    const memTotal = pick(r, [8, 16, 16, 32]) * 1024 ** 3;
    const diskTotal = pick(r, [256, 512, 512, 1024]) * 1000 ** 3;
    // A handful of hosts are genuinely tight on disk — enough for the
    // check to have something to say, few enough that it reads as an
    // alert rather than as the fleet's normal condition. Split into two
    // fixed bands rather than left to the PRNG: whether the demo shows
    // any RED at all shouldn't depend on where the random draws landed.
    const criticalDisk = i % 97 === 5; // 3 hosts, under the 5% fail line
    const tightDisk = !criticalDisk && i % 37 === 5; // 6 more, in the warn band

    const hostId = isServer
      ? `${ORG}-SV-${String(++svSeq).padStart(4, '0')}`
      : isOpsTerminal
        ? `${ORG}-MT-${String(++mtSeq).padStart(4, '0')}`
        : `${ORG}-PC-${String(++pcSeq).padStart(4, '0')}`;

    out.push({
      pc_id: hostId,
      command_keys: RING,
      enforcing: true,
      hostname: hostId,
      os_family: linux ? 'linux' : 'windows',
      agent_version: version,
      online,
      // Online hosts heartbeat within the last ~90s; offline ones went
      // quiet somewhere between an hour and four days ago.
      heartbeat_ms_ago: online
        ? int(r, 3, 90) * 1000
        : int(r, 60, 5760) * 60 * 1000,
      cpu_pct: Math.round((3 + r() * (i % 23 === 0 ? 80 : 22)) * 10) / 10,
      mem_used_bytes: Math.round(memTotal * (0.34 + r() * 0.42)),
      mem_total_bytes: memTotal,
      disk_total_bytes: diskTotal,
      // A FRACTION of this host's own disk, never an independent draw.
      // Drawing free space on its own handed a 256 GB laptop 410 GB
      // free, and three separate places divide the two: the Inventory
      // page shows them side by side, the `disk_free` check prints the
      // ratio as its detail, and the low-disk analytics table sorts on
      // it. A ratio above 1 is the loudest possible tell that the data
      // is invented.
      //
      // Healthy by default with a deliberate handful of tight hosts.
      // An earlier draft spread free space uniformly from 4%, which put
      // ~20 machines under the threshold — a fleet where 8% of the
      // estate is about to run out of disk reads as neglected, not as a
      // product catching the two boxes that need attention this week.
      disk_free_bytes: Math.round(
        diskTotal *
          (criticalDisk ? 0.02 + r() * 0.025 : tightDisk ? 0.07 + r() * 0.11 : 0.22 + r() * 0.5),
      ),
      agent_cpu_pct: Math.round(r() * 25) / 10,
      agent_rss_bytes: int(r, 22, 48) * 1024 * 1024,
      last_logon_user: `${ORG}\\${account}`,
      last_logon_display_name: `${surname} ${given}`,
      // Account and address are ONE identity, from one string. They were
      // built separately — the account carried a numeric suffix and the
      // address did not — so 248 hosts shared 15 addresses and one of
      // them belonged to 21 different named employees. Inventory shows
      // name and email side by side, so that fits in one screenshot:
      // the same class of tell as a disk with more free space than
      // capacity, which this file already guards against.
      email: `${account}@example.co.jp`,
      dept,
      site,
      model: isServer
        ? pick(r, SERVER_MODELS)
        : linux
          ? 'OptiPlex 7010'
          : pick(r, MODELS),
      serial: `JP${int(r, 10000000, 99999999)}`,
      os_caption: linux ? 'Ubuntu 24.04.1 LTS' : os.caption,
      os_version: linux ? '24.04' : os.version,
      os_build: linux ? '6.8.0-51-generic' : os.build,
      // Membership is DERIVED from this host's own meta (site, dept) plus
      // two hand-picked operational sets. `server.ts` resolves the group
      // DEFINITIONS against the same fields, so the definition page and the
      // membership page cannot disagree about who is in what.
      groups: [
        dept,
        site,
        ...(i % 23 === 0 ? ['高負荷監視'] : []),
        // Static membership: machines the IT team works FROM, and the few
        // always-on boxes. Neither is expressible as a query over meta —
        // which is exactly why the product has static groups at all.
        ...(isOpsTerminal ? ['情シス運用端末'] : []),
        ...(isServer ? ['サーバー'] : []),
      ],
    });
  }
  return out;
}

export const FLEET: DemoPc[] = build();

export const ONLINE = FLEET.filter((p) => p.online);
export const OFFLINE = FLEET.filter((p) => !p.online);

// ---------------------------------------------------------------- jobs

/** Registered job manifests the demo fleet runs on a schedule. */
export const JOBS = [
  { id: 'inventory-basic', label: 'ハードウェア/OS インベントリ', every: '6h' },
  { id: 'inventory-apps', label: 'インストール済みアプリ収集', every: '12h' },
  { id: 'defender-status', label: 'Defender 稼働確認', every: '1h' },
  { id: 'bitlocker-status', label: 'BitLocker 暗号化状態', every: '12h' },
  { id: 'windows-update', label: 'Windows Update 適用状況', every: '6h' },
  { id: 'disk-space', label: 'ディスク空き容量', every: '1h' },
  { id: 'app-usage', label: 'アプリ利用時間の集計', every: '15m' },
  { id: 'web-history', label: 'ブラウザ閲覧履歴の収集', every: '30m' },
  { id: 'edge-extensions', label: 'Edge 拡張機能の棚卸し', every: '24h' },
] as const;

/** Compliance checks, with a deliberately imperfect fleet: an all-green
 *  board photographs well but tells the viewer nothing about what the
 *  product is for. */
/**
 * The two data-driven checks classify each host ONCE, here, and both the
 * tallies below and the per-host attention rows in `server.ts` read that
 * same function.
 *
 * They used to be written twice — a count in this file and a row-picking
 * filter in the server — and the two drifted: the count said "expires
 * within 6 months" while the row filter said "not yet expired", so the
 * board tallied 27 warnings and then listed seven hosts whose support
 * ends in 2027. A number and the list under it disagreeing is the exact
 * failure this fixture is supposed to be immune to.
 */
const DISK_FAIL_RATIO = 0.05;
const DISK_WARN_RATIO = 0.2;

export function diskBucket(p: DemoPc): 'ok' | 'warn' | 'fail' {
  const ratio = p.disk_free_bytes / p.disk_total_bytes;
  if (ratio < DISK_FAIL_RATIO) return 'fail';
  return ratio < DISK_WARN_RATIO ? 'warn' : 'ok';
}

// Evaluated against the wall clock, not hardcoded, so the EOL story
// stays true as the demo ages: once 2026-10-13 passes, 24H2 moves from
// the warn bucket to the fail bucket on its own and the detail line
// follows. A pinned "today" would quietly turn this check into fiction.
const today = () => new Date().toISOString().slice(0, 10);
const EOL_HORIZON_DAYS = 183;

export function osEolBucket(p: DemoPc): 'ok' | 'warn' | 'fail' {
  const eol = p.os_family === 'windows' ? OS_EOL_DATE[p.os_version] : undefined;
  if (!eol) return 'ok';
  const t = today();
  if (eol < t) return 'fail';
  const horizon = new Date(Date.now() + EOL_HORIZON_DAYS * 86_400_000)
    .toISOString()
    .slice(0, 10);
  // Support ending in a year is not something to act on this quarter.
  return eol <= horizon ? 'warn' : 'ok';
}

const tally = (f: (p: DemoPc) => 'ok' | 'warn' | 'fail') => ({
  ok: FLEET.filter((p) => f(p) === 'ok').length,
  warn: FLEET.filter((p) => f(p) === 'warn').length,
  fail: FLEET.filter((p) => f(p) === 'fail').length,
  unknown: 0,
});

export const CHECKS = [
  { name: 'defender_realtime', label: 'Defender リアルタイム保護', ok: FLEET.length - 2, warn: 0, fail: 2, unknown: 0 },
  { name: 'bitlocker', label: 'BitLocker 暗号化', ok: FLEET.length - 11, warn: 5, fail: 4, unknown: 2 },
  { name: 'windows_update', label: 'Windows Update 適用状況', ok: FLEET.length - 16, warn: 12, fail: 2, unknown: 2 },
  { name: 'firewall', label: 'Windows ファイアウォール', ok: FLEET.length, warn: 0, fail: 0, unknown: 0 },
  { name: 'screen_lock', label: 'スクリーンロック (15分)', ok: FLEET.length - 6, warn: 6, fail: 0, unknown: 0 },
  { name: 'edge_extensions', label: 'Edge 拡張機能ポリシー', ok: FLEET.length - 5, warn: 3, fail: 2, unknown: 0 },
  // Counted, not invented — same rule as `os_eol` below. The detail line
  // prints each host's real free-space ratio, so a hand-written tally
  // would contradict both the Inventory page and the low-disk table.
  { name: 'disk_free', label: 'ディスク空き容量 (20%)', ...tally(diskBucket) },
  // Also counted. `warn` is "expires within 6 months" and `fail` is
  // "already expired", both read off the real servicing calendar in
  // OS_EOL_DATE, so the numbers move correctly if the OS mix is edited.
  { name: 'os_eol', label: 'OS サポート期限', ...tally(osEolBucket) },
];
