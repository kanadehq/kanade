/**
 * Capture documentation / promo screenshots from the demo stack.
 *
 * Run from crates/kanade-backend/web with `cargo make demo` already serving
 * on BASE. Every shot is taken at deviceScaleFactor 2, so a 1440x900 frame
 * lands as 2880x1800 real pixels — enough for a slide deck; the book copies
 * are downscaled from these.
 *
 * Auth, theme and language are injected into localStorage before any page
 * script runs, so no shot has to walk through the login form.
 */
import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const BASE = process.env.BASE ?? 'http://localhost:5173';
const OUT = process.env.OUT ?? 'C:/Users/yukimemi/kanade-promo-shots';
const ONLY = process.env.ONLY ? new RegExp(process.env.ONLY) : null;

const DESKTOP = { width: 1440, height: 900 };
const MOBILE = { width: 390, height: 901 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Settle: let data land, then kill motion so frames are reproducible. */
async function settle(page, extra = 0) {
  try {
    await page.waitForLoadState('networkidle', { timeout: 12000 });
  } catch {
    /* polling pages never go idle; the fixed wait below covers them */
  }
  await page.addStyleTag({
    content: '*,*::before,*::after{transition:none!important;animation:none!important;caret-color:transparent!important}',
  });
  await sleep(600 + extra);
}

/**
 * Scroll the card carrying `label` to the top of the viewport.
 *
 * Matches a LEAF element whose whole text is the label, then walks up to the
 * card. Matching on `textContent.includes` instead walks the tree top-down
 * and hits an ancestor first — the container that holds every card matches
 * too, and scrolling that lands on the top of the page rather than the card
 * you asked for.
 */
async function scrollToLabel(page, label, block = 'start') {
  return page.evaluate(
    ([n, b]) => {
      const leaf = [...document.querySelectorAll('main *')].find(
        (e) => e.children.length === 0 && e.textContent?.trim() === n,
      );
      const card = leaf?.closest('div.rounded-lg');
      if (!card) return false;
      card.scrollIntoView({ block: b });
      return true;
    },
    [label, block],
  );
}

/**
 * Open the History tab of ONE manifest's card.
 *
 * Scoped to the named card on purpose: the first card on the page is
 * `inventory-basic`, whose history is empty by design, so an unscoped
 * "click the History tab" lands on an empty-state panel.
 *
 * `since` is selected by option VALUE (`24h|7d|30d|90d|all`), not by label —
 * the labels are localised, so matching on text works in one locale only.
 */
async function openHistory(page, manifest, since) {
  const card = page.locator('div.rounded-lg').filter({ hasText: manifest }).first();
  await card.scrollIntoViewIfNeeded();
  const tabs = card.locator('button[aria-pressed]');
  await tabs.nth(1).click();
  await sleep(900);
  if (since) {
    await card.locator('select').first().selectOption(since);
    await sleep(900);
  }
  await card.scrollIntoViewIfNeeded();
  await page.evaluate(
    (m) => {
      const hit = [...document.querySelectorAll('div.rounded-lg')].find((e) =>
        e.textContent?.includes(m),
      );
      hit?.scrollIntoView({ block: 'start' });
    },
    manifest,
  );
  await sleep(300);
}

let ids = {};

// name → optional async prep(page). Path may be a string or a function.
const SHOTS = [
  // ---- overview pages -------------------------------------------------
  { name: 'dashboard', path: '/dashboard' },
  {
    name: 'dashboard-widgets',
    path: '/dashboard',
    // Scroll to the first chart card rather than a label: the widget titles
    // are operator-authored, so there is no stable string to aim at.
    prep: (p) =>
      p.evaluate(() => {
        const c = document.querySelector('.recharts-responsive-container');
        c?.closest('div.rounded-lg')?.scrollIntoView({ block: 'center' });
      }),
  },
  { name: 'agents', path: '/agents' },
  { name: 'inventory', path: '/inventory' },
  { name: 'compliance', path: '/compliance' },
  { name: 'activity', path: '/activity' },
  {
    name: 'events',
    // Explicit window and row cap, not the page defaults. The default is
    // "2 days back to midnight", so a capture taken at a weekend shows only
    // the weekend: the fixture runs a skeleton on-call crew then, which is
    // 2 hosts working ~2.5 hours and reads as "uptime tracking is broken".
    // 7 days always contains a working day whatever day this is run, and
    // limit=1000 clears the fixture's ~344 rows so no lane is hatched as
    // truncated.
    path: '/events?since=7d&limit=1000',
    settleExtra: 600,
  },
  { name: 'audit', path: '/audit' },
  { name: 'jobs', path: '/jobs' },
  {
    name: 'jobs-yaml',
    path: '/jobs',
    // The aria-label IS translated (`actions.editAria`), so match both
    // locales rather than the English string alone.
    prep: async (p) => {
      await p
        .getByRole('button', { name: /edit job|を編集/ })
        .first()
        .click();
      await sleep(1800); // Monaco loads its worker before it paints
    },
  },
  { name: 'schedules', path: '/schedules' },
  { name: 'views', path: '/views' },
  { name: 'groups', path: '/groups' },
  { name: 'notifications', path: '/notifications' },
  { name: 'collect', path: '/collect' },
  { name: 'rollout', path: '/rollout' },
  { name: 'exec', path: () => `/exec?job_id=${ids.job}` },
  { name: 'run', path: '/run' },
  { name: 'jetstream', path: '/jetstream' },
  { name: 'accounts', path: '/accounts' },

  // ---- self-service account: TOTP + password (#1192 / #1227) -----------
  // Order matters: the demo backend keeps enrolment state, so the "off"
  // and enrolling frames have to be taken before the one that turns it on,
  // and that one resets the flag afterwards for the next locale pass.
  { name: 'account', path: '/account' },
  {
    name: 'account-mfa-enroll',
    path: '/account',
    prep: async (p) => {
      // The MFA card is the first card on the page and its only button at
      // rest is "enable" — addressed by position because the label is
      // localised.
      await p.locator('div.rounded-lg').first().getByRole('button').first().click();
      await sleep(1200); // QR renders after mfa/init resolves
    },
  },
  {
    name: 'account-mfa-on',
    path: '/account',
    prep: async (p) => {
      const card = p.locator('div.rounded-lg').first();
      await card.getByRole('button').first().click();
      await sleep(1000);
      // nth(1), not first(): input 0 is the read-only setup key, and
      // filling a read-only field just times out.
      const code = card.locator('input').nth(1);
      await code.fill('123456');
      await code.press('Enter'); // submits the enrolment form
      await sleep(1400);
    },
    after: (p) => p.request.post(BASE + '/api/auth/mfa/disable'),
  },
  { name: 'config', path: '/config' },
  { name: 'logs', path: () => `/logs?pc=${ids.pc}`, settleExtra: 900 },

  // ---- agent detail ---------------------------------------------------
  { name: 'agent-detail', path: () => `/agents/${ids.pc}` },
  {
    name: 'agent-detail-perf',
    path: () => `/agents/${ids.pc}`,
    prep: async (p) => {
      await p.evaluate(() => {
        const c = document.querySelector('.recharts-responsive-container');
        c?.closest('[class*="rounded-lg"]')?.scrollIntoView({ block: 'start' });
      });
    },
  },
  {
    // Operator-attached key/value metadata (#agent-meta): the card an
    // operator edits by hand, and the one the AD sync job writes into.
    name: 'agent-detail-meta',
    path: () => `/agents/${ids.pc}`,
    prep: (p) =>
      p.evaluate(() => {
        // Heading text is localised, so match both rather than one.
        const h = [...document.querySelectorAll('main h3')].find((x) =>
          /attributes|付加情報/i.test(x.textContent ?? ''),
        );
        h?.closest('div.rounded-lg')?.scrollIntoView({ block: 'start' });
      }),
  },
  {
    name: 'agent-detail-processes',
    path: () => `/agents/${ids.pc}`,
    prep: async (p) => {
      await p.evaluate(() => {
        const t = [...document.querySelectorAll('table')].pop();
        t?.closest('[class*="rounded-lg"]')?.scrollIntoView({ block: 'start' });
      });
    },
  },

  {
    // The remote screen only exists as a live socket, so this frame is the
    // one that needs a click: the page opens idle over a black canvas and
    // connects on demand.
    name: 'remote',
    path: () => `/remote/${ids.pc}`,
    settleExtra: 400,
    prep: async (p) => {
      // The connect button is the trailing action in the header; the label
      // is localised, and the back control precedes it.
      await p.getByRole('button').last().click();
      // Wait for a PAINTED canvas, not merely a sized one. The viewer sets
      // width/height from the tile meta and only then awaits
      // `createImageBitmap`, so a size check passes before anything is drawn
      // — which is the black rectangle this shot exists to replace. A fresh
      // canvas is transparent, so a non-zero alpha at the centre is the
      // cheapest proof that a tile actually landed.
      await p.waitForFunction(() => {
        const c = document.querySelector('canvas');
        if (!c || c.width <= 1 || c.height <= 1) return false;
        const px = c
          .getContext('2d')
          ?.getImageData(Math.floor(c.width / 2), Math.floor(c.height / 2), 1, 1).data;
        return !!px && px[3] !== 0;
      }, null, { timeout: 15000 });
      // Long enough for a few more tiles to land: a counter reading "1" makes
      // a live session look like a single still.
      await sleep(4600);
    },
  },

  // ---- inventory drill-down + history ---------------------------------
  {
    name: 'inventory-pc',
    path: () => `/inventory?pc=${ids.pc}&job=inventory-apps`,
    settleExtra: 800,
  },
  {
    name: 'inventory-history',
    path: () => `/inventory?pc=${ids.pc}&job=inventory-apps`,
    settleExtra: 800,
    prep: (p) => openHistory(p, 'inventory-apps'),
  },
  {
    name: 'inventory-history-all',
    path: () => `/inventory?pc=${ids.pc}&job=inventory-apps`,
    settleExtra: 800,
    // The whole window, so an install, an upgrade and a removal are all
    // on screen at once — the default 7 d shows only the newest few.
    prep: (p) => openHistory(p, 'inventory-apps', 'all'),
  },

  // ---- compliance, below the fold -------------------------------------
  {
    name: 'compliance-os-eol',
    path: '/compliance',
    prep: (p) => scrollToLabel(p, 'os_eol'),
  },
  {
    name: 'compliance-bitlocker',
    path: '/compliance',
    prep: (p) => scrollToLabel(p, 'bitlocker'),
  },
  {
    name: 'compliance-windows-update',
    path: '/compliance',
    prep: (p) => scrollToLabel(p, 'windows_update'),
  },

  // ---- analytics tabs --------------------------------------------------
  { name: 'analytics-app-usage', path: '/analytics', settleExtra: 500 },
  {
    name: 'analytics-inventory',
    path: '/analytics',
    settleExtra: 500,
    prep: async (p) => {
      await p.getByRole('button', { name: 'inventory', exact: true }).click().catch(() => {});
      await sleep(900);
    },
  },
  {
    name: 'analytics-web-history',
    path: '/analytics',
    settleExtra: 500,
    prep: async (p) => {
      await p.getByRole('button', { name: 'web-history', exact: true }).click().catch(() => {});
      await sleep(900);
    },
  },

  // ---- result / notification detail -----------------------------------
  { name: 'activity-detail', path: () => `/activity/${ids.result}` },
  { name: 'notification-detail', path: () => `/notifications/${ids.notification}`, settleExtra: 400 },
  {
    // The audience table is where the three ack states live side by side:
    // confirmed, retracted, and not-yet-confirmed, each against the user who
    // was on the machine. Rows are grouped by state, so framing the boundary
    // is what gets all three into one frame — the single retracted row is
    // otherwise buried in sixty.
    name: 'notification-detail-audience',
    path: () => `/notifications/${ids.notification}`,
    settleExtra: 400,
    prep: (p) =>
      p.evaluate(() => {
        const t = [...document.querySelectorAll('table')].find(
          (x) => x.querySelectorAll('tbody tr').length > 30,
        );
        if (!t) return;
        const rows = [...t.querySelectorAll('tbody tr')];
        const status = (r) => r.children[2]?.textContent?.trim();
        // Find the first state change rather than matching a status string —
        // the labels are localised, the grouping is not.
        const first = status(rows[0]);
        const i = rows.findIndex((r) => status(r) !== first);
        rows[Math.max(0, i - 6)]?.scrollIntoView({ block: 'start' });
      }),
  },
  {
    name: 'notification-detail-acks',
    path: () => `/notifications/${ids.notification}`,
    settleExtra: 400,
    prep: async (p) => {
      await p.evaluate(() => {
        const t = [...document.querySelectorAll('table')].pop();
        t?.closest('[class*="rounded-lg"]')?.scrollIntoView({ block: 'start' });
      });
    },
  },
];

// Shots worth a dark-theme and a phone frame too.
const DARK = new Set([
  'dashboard', 'dashboard-widgets', 'agents', 'compliance', 'compliance-os-eol',
  'inventory-history-all', 'analytics-inventory', 'jetstream', 'notification-detail', 'notification-detail-audience',
  'remote',
  'account-mfa-enroll',
  'agent-detail-perf', 'events', 'audit',
]);
const MOBILE_SET = new Set([
  'dashboard', 'agents', 'compliance', 'inventory', 'notifications', 'analytics-app-usage',
]);

async function run() {
  fs.mkdirSync(OUT, { recursive: true });

  const j = async (p) => (await fetch(BASE + p)).json();
  const agents = await j('/api/agents?limit=5');
  const results = await j('/api/results?limit=20');
  const notes = await j('/api/notifications');
  ids = {
    pc: agents[0]?.pc_id,
    // Prefer a failed run: a non-zero exit tells a better story than "ok".
    result: (results.find((r) => r.exit_code !== 0) ?? results[0])?.result_id,
    job: (await j('/api/jobs'))[0]?.id ?? 'inventory-basic',
    notification: notes[0]?.id,
  };
  console.log('ids:', JSON.stringify(ids));

  const browser = await chromium.launch();
  const failures = [];
  let made = 0;

  for (const locale of ['ja', 'en']) {
    for (const variant of ['light', 'dark', 'mobile']) {
      const theme = variant === 'dark' ? 'dark' : 'light';
      const ctx = await browser.newContext({
        viewport: variant === 'mobile' ? MOBILE : DESKTOP,
        deviceScaleFactor: 2,
        isMobile: variant === 'mobile',
        hasTouch: variant === 'mobile',
        locale: locale === 'ja' ? 'ja-JP' : 'en-US',
        colorScheme: theme,
      });
      await ctx.addInitScript(
        ([t, l]) => {
          localStorage.setItem('kanade_token', 'demo-token');
          localStorage.setItem('kanade-theme', t);
          localStorage.setItem('kanade_lang', l);
        },
        [theme, locale],
      );
      const page = await ctx.newPage();

      for (const s of SHOTS) {
        if (variant === 'dark' && !DARK.has(s.name)) continue;
        if (variant === 'mobile' && !MOBILE_SET.has(s.name)) continue;
        const file = `${s.name}-${variant}-${locale}.jpg`;
        if (ONLY && !ONLY.test(file)) continue;
        const url = BASE + (typeof s.path === 'function' ? s.path() : s.path);
        try {
          await page.goto(url, { waitUntil: 'domcontentloaded' });
          await settle(page, s.settleExtra ?? 0);
          if (s.prep) await s.prep(page);
          await page.screenshot({
            path: path.join(OUT, file),
            type: 'jpeg',
            quality: 92,
          });
          made++;
          console.log('ok   ' + file);
        } catch (e) {
          failures.push(`${file}: ${e.message.split('\n')[0]}`);
          console.log('FAIL ' + file + ' — ' + e.message.split('\n')[0]);
        } finally {
          // `finally`, not the happy path: a shot that mutates demo state has
          // to undo it even when it fails midway. `account-mfa-on` throwing
          // after `mfa/verify` would otherwise leave the demo backend with
          // MFA on, and every later account frame — including the other
          // locale's — would capture the wrong state. Swallow errors here so
          // a failing cleanup cannot mask the failure that caused it.
          if (s.after) await s.after(page).catch(() => {});
        }
      }
      await ctx.close();
    }
  }
  await browser.close();

  console.log(`\n${made} shots written to ${OUT}`);
  if (failures.length) {
    console.log(`${failures.length} failures:`);
    for (const f of failures) console.log('  ' + f);
    // A partial set must not read as success — the caller is usually about
    // to copy these straight into the book.
    process.exitCode = 1;
  }
}

run();
