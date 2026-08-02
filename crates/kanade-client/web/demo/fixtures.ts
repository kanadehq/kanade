/**
 * The invented single-PC state behind the Client App demo
 * (`cargo make demo-client`).
 *
 * This is the END USER's half of the product. The SPA demo shows what an
 * administrator sees across a fleet; this shows what lands on the desk of
 * the person whose PC it is — the health tab they check, the self-service
 * jobs they can run, the notice they have to confirm. For a promo page
 * that is arguably the more persuasive of the two: it answers "what
 * changes for my staff if we deploy this".
 *
 * Everything here is fiction. The point is the same as the SPA demo's:
 * produce screenshots and walkthroughs without pointing anything at a
 * real machine, which would put a real hostname and a real sign-in name
 * into whatever the image ends up in.
 *
 * Timestamps are OFFSETS resolved against the wall clock at call time,
 * never absolute — otherwise every "3分前" in the panel drifts into
 * nonsense the longer the demo runs.
 */

export const PC_ID = 'KANADE-PC-0001';
/** One version for the whole product.
 *
 * The client, the agent and the backend are released together out of one
 * workspace version, so the sidebar's build badge, `state_snapshot`'s
 * `agent_version` and the SPA demo's `/api/version` are all this number.
 * It was called AGENT_VERSION while it also stood in for the CLIENT's own
 * `app_version`, which made the fixture read as if two things happened to
 * agree rather than being one thing. */
export const PRODUCT_VERSION = '1.0.0';
export const DISPLAY_NAME = '端末管理支援ツール';

export const iso = (msAgo: number) => new Date(Date.now() - msAgo).toISOString();
export const isoIn = (msAhead: number) => new Date(Date.now() + msAhead).toISOString();

// ---------------------------------------------------------------- health

/**
 * The Health tab. Deliberately not all-green: one warning is what makes
 * the tab worth opening, and it is the row that shows the product's real
 * shape — a check is operator-authored, so it carries an explanation and
 * a troubleshooting hint rather than a bare red dot.
 */
export const CHECKS = [
  {
    name: 'defender_realtime',
    label: 'ウイルス対策',
    status: 'ok' as const,
    detail: 'リアルタイム保護は有効です',
  },
  {
    name: 'bitlocker',
    label: 'ディスク暗号化',
    status: 'ok' as const,
    detail: 'C: は保護されています (XtsAes256)',
  },
  {
    name: 'windows_update',
    label: 'Windows Update',
    status: 'warn' as const,
    detail: '再起動待ちの更新が 1 件あります',
    troubleshoot:
      'スタート → 電源 → 「更新して再起動」を選ぶと適用されます。所要 5 分程度です。',
  },
  {
    name: 'firewall',
    label: 'ファイアウォール',
    status: 'ok' as const,
    detail: '有効',
  },
  {
    name: 'disk_free',
    label: 'ディスク空き容量',
    status: 'ok' as const,
    detail: '空き 281 GB (28%)',
  },
];

// ------------------------------------------------------------------ jobs

/**
 * The self-service catalog. `category` drives the tabs, and the mix is
 * chosen so the demo shows all three states an operator can configure:
 * a job that runs immediately, one that asks first (`confirm`), and one
 * that is only visible because a support grant unlocked it.
 */
export const JOBS = [
  {
    id: 'windows-update-run',
    display_name: 'Windows Update を実行',
    display_description: '未適用の更新を確認して適用します。再起動が必要な場合があります。',
    icon: 'download',
    category: 'software_update',
    version: '1.2.0',
    timeout_secs: 1800,
    confirm: {
      enabled: true,
      message: '更新の適用には数分かかり、再起動が必要になる場合があります。実行しますか?',
    },
  },
  {
    id: 'printer-reset',
    display_name: 'プリンターをリセット',
    display_description: '印刷キューを空にして、印刷スプーラーを再起動します。',
    icon: 'wrench',
    category: 'troubleshoot',
    version: '1.0.3',
    timeout_secs: 120,
  },
  {
    id: 'network-diagnose',
    display_name: 'ネットワーク診断',
    display_description: '接続状況を確認し、結果を情報システム部へ送信します。',
    icon: 'wrench',
    category: 'troubleshoot',
    version: '1.1.0',
    timeout_secs: 300,
    // No confirm block ⇒ the historical default (modal with the built-in
    // message). Left as-is on purpose so the demo shows that path too.
  },
  {
    id: 'teams-reinstall',
    display_name: 'Microsoft Teams を再インストール',
    display_description: '起動しない場合に、いったん削除して入れ直します。',
    icon: 'package',
    category: 'catalog',
    version: '2.0.1',
    timeout_secs: 900,
    confirm: { enabled: true, message: 'Teams を再インストールします。作業中のチャットは保存してください。' },
  },
  {
    id: 'collect-support-logs',
    display_name: 'サポート用ログを収集',
    display_description: '調査用のログをまとめて送信します。担当者の指示があるときに実行してください。',
    icon: 'wrench',
    category: 'troubleshoot',
    version: '1.0.0',
    timeout_secs: 600,
    // Only listed once a support grant is live — the agent applies the
    // gate when it builds the list, so the demo does the same.
    unlock: 'support',
  },
];

// --------------------------------------------------------- notifications

/**
 * Bodies are Markdown, rendered through the same `marked` + DOMPurify
 * allowlist the SPA previews with — headings included, since #1262.
 *
 * KEPT BYTE-IDENTICAL to `crates/kanade-backend/web/demo/server.ts`.
 * The operator sends these and the end user reads them here, so the two
 * demos are two views of ONE notice. They drifted once, because each
 * was written in its own PR with nothing tying them together: same
 * titles, different bodies, the SPA's the fuller of the two. Screenshots
 * placed side by side would have contradicted each other. If you edit a
 * body, edit both.
 */
const BR = '\n';

export const NOTIFICATIONS = [
  {
    id: 'ntf-0003',
    priority: 'warn' as const,
    require_ack: true,
    title: '【重要】月次セキュリティ更新の適用について',
    body: [
      '**本日 18:00 以降**、Windows Update の適用と再起動をお願いします。',
      '',
      '作業中のファイルは必ず保存してください。再起動は自動では行われません。',
      '',
      '## 手順',
      '',
      '1. スタート → 設定 → Windows Update',
      '2. 「更新プログラムのチェック」を実行',
      '3. 表示された更新をすべて適用',
      '4. 求められたら再起動',
      '',
      '## 拠点ごとの推奨時間帯',
      '',
      '| 拠点 | 推奨時間帯 |',
      '| --- | --- |',
      '| 東京本社 | 18:00 - 20:00 |',
      '| 大阪支社 | 18:30 - 20:30 |',
      '| その他拠点 | 業務終了後いつでも |',
      '',
      '> 再起動後に不具合が出た場合は、**適用を取り消さず**に情報システム部へご連絡ください。',
      '',
      '手順の詳細は [社内ポータルの手順書](https://portal.example.co.jp/it/windows-update) を参照してください。',
    ].join(BR),
    // NOT toasted, deliberately. The app re-toasts every unacked
    // `toast: true` notice on connect, so leaving this one on meant a
    // toast fired the instant the window appeared — the two arrived
    // together and the toast read as part of the app launching rather
    // than as something reaching the user on its own. It still sits in
    // the panel unconfirmed, which is what makes 確認 worth showing;
    // the live arrival is `PUSHED_NOTIFICATION`'s job, a few seconds
    // later, with nothing else happening on screen.
    toast: false,
    issued_ms_ago: 3 * 3600 * 1000,
    issued_by: 'jyoho-sys',
    expires_in_ms: 2 * 86_400_000,
    acked_ms_ago: null as number | null,
  },
  {
    id: 'ntf-0002',
    priority: 'info' as const,
    require_ack: false,
    title: '社内 Wi-Fi メンテナンスのお知らせ',
    body: [
      '今週土曜 **22:00 〜 翌 2:00** に無線 LAN のメンテナンスを実施します。',
      '',
      '対象は以下の SSID です。時間中は接続が断続的に切れます。',
      '',
      '- `KANADE-CORP` — 全拠点',
      '- `KANADE-GUEST` — 東京本社のみ',
      '',
      '有線 LAN と VPN は影響を受けません。**作業予定のある方は有線をご利用ください。**',
      '',
      '進捗は [ステータスページ](https://portal.example.co.jp/it/status) で随時更新します。',
    ].join(BR),
    toast: false,
    issued_ms_ago: 28 * 3600 * 1000,
    issued_by: 'jyoho-sys',
    expires_in_ms: null,
    acked_ms_ago: null as number | null,
  },
  {
    id: 'ntf-0001',
    priority: 'emergency' as const,
    require_ack: true,
    title: '不審メールにご注意ください',
    body: [
      '請求書を装った**添付ファイル付きメール**が複数届いています。',
      '',
      '**開かないでください。** 添付を開くと端末が暗号化される恐れがあります。',
      '',
      '## 見分け方',
      '',
      '- 差出人が取引先に似ているが、ドメインが 1 文字違う',
      '- 件名に「請求書」「支払い」「至急」を含む',
      '- 添付が `.zip` または `.iso`',
      '',
      '## 該当メールを受け取ったら',
      '',
      '1. 開かない・返信しない',
      '2. `security@example.co.jp` へ**添付したまま転送**',
      '3. 転送後、元のメールを削除',
      '',
      '> すでに開いてしまった場合は、**端末をネットワークから切断**したうえで内線 1234 までご連絡ください。',
    ].join(BR),
    toast: true,
    issued_ms_ago: 4 * 86_400_000,
    issued_by: 'jyoho-sys',
    expires_in_ms: null,
    // Already confirmed — the panel needs to show both states, or the
    // demo only ever shows the "you have something to do" half.
    acked_ms_ago: 3.5 * 86_400_000,
  },
];

/**
 * The notice that arrives WHILE the demo is open, pushed a few seconds
 * after connect. A static panel can show that notifications exist; only
 * a live one shows what actually happens on the user's screen when the
 * operator hits send.
 */
export const PUSHED_NOTIFICATION = {
  id: 'ntf-0004',
  priority: 'info' as const,
  require_ack: false,
  title: 'ヘルプデスク受付時間の変更',
  body: [
    '来週より、ヘルプデスクの受付時間を **9:00 〜 18:00** に変更します。',
    '',
    '時間外の連絡は [問い合わせフォーム](https://portal.example.co.jp/it/contact) をご利用ください。',
  ].join(BR),
  toast: true,
  issued_by: 'jyoho-sys',
};

/** Seconds after connect before the pushed notice arrives. */
export const PUSH_DELAY_MS = 8_000;

// ------------------------------------------------------------- job output

/** What each job prints while it runs, as [delayMs, line] pairs. */
export const JOB_OUTPUT: Record<string, Array<[number, string]>> = {
  'windows-update-run': [
    [300, '更新を検索しています...'],
    [1400, '3 件の更新が見つかりました'],
    [2200, '  - 2026-07 x64 ベース システム用 累積更新プログラム (KB5062553)'],
    [2600, '  - Microsoft Defender 定義更新 (KB2267602)'],
    [3000, '  - .NET Framework 用 累積更新 (KB5062040)'],
    [4200, 'ダウンロード中... 100%'],
    [6000, 'インストール中... 100%'],
    [7000, '完了しました。再起動が必要です。'],
  ],
  'printer-reset': [
    [300, '印刷スプーラーを停止しています...'],
    [1200, '印刷キューを削除しました (2 件)'],
    [2000, '印刷スプーラーを開始しています...'],
    [2800, '完了しました。'],
  ],
  'network-diagnose': [
    [300, '既定のゲートウェイに ping しています...'],
    [1100, '  192.168.10.1 — 応答あり (1ms)'],
    [1900, 'DNS を確認しています...'],
    [2600, '  portal.example.co.jp — 解決成功'],
    [3400, '社内ポータルへの疎通を確認しています...'],
    [4200, '  HTTP 200 (128ms)'],
    [5000, '問題は見つかりませんでした。結果を送信しました。'],
  ],
  'teams-reinstall': [
    [300, '既存のインストールを削除しています...'],
    [2500, 'インストーラーをダウンロードしています...'],
    [5000, 'インストール中...'],
    [8000, '完了しました。Teams を起動してください。'],
  ],
  'collect-support-logs': [
    [300, 'イベントログを収集しています...'],
    [1800, 'エージェントのログを収集しています...'],
    [3000, 'アップロード中...'],
    [4200, '完了しました。担当者に連絡してください。'],
  ],
};

/** The passcode the demo's support unlock accepts. */
export const SUPPORT_CODE = '123456';
