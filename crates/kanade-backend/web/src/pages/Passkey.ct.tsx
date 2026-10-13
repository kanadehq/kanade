import { expect, test as base } from '@playwright/experimental-ct-react';

import { CardHost, LoginHost } from './Passkey.ct.harness';

// Browser-level coverage for passkey sign-in and management (Chromium only,
// matching playwright-ct.config.ts — Firefox/WebKit are NOT exercised here).
//
// A CDP *virtual authenticator* stands in for the device: it is a real
// WebAuthn implementation inside the browser (resident keys, user
// verification), so `navigator.credentials` genuinely runs and our
// base64url/JSON conversion is tested against real browser output, not a
// stub. The backend is mocked at the network layer; its half of the
// protocol (signatures, origin, replay, throttling) is covered by the Rust
// suite with a software authenticator.

const RP_ID = 'localhost';
const b64url = (s: string) => Buffer.from(s).toString('base64url');

type Captured = { url: string; body: any };
type Handler = (body: any) => { status?: number; json?: unknown };
type Cdp = { send(method: string, params?: object): Promise<any> };

const test = base.extend<{
  /** Attach a virtual device. Chrome allows one internal authenticator per
   *  browser, so it is detached again after each test. */
  virtualAuthenticator: (opts?: {
    verified?: boolean;
  }) => Promise<{ cdp: Cdp; authenticatorId: string }>;
  /** Mock the JSON API; every request that reaches a handler is appended to `log`. */
  mockApi: (log: Captured[], routes: Record<string, Handler>) => Promise<void>;
}>({
  virtualAuthenticator: async ({ page }, use) => {
    let attached: { cdp: Cdp; authenticatorId: string } | undefined;
    await use(async (opts = {}) => {
      const cdp = await page.context().newCDPSession(page);
      await cdp.send('WebAuthn.enable');
      const { authenticatorId } = await cdp.send('WebAuthn.addVirtualAuthenticator', {
        options: {
          protocol: 'ctap2',
          transport: 'internal',
          hasResidentKey: true,
          hasUserVerification: true,
          isUserVerified: opts.verified ?? true,
          automaticPresenceSimulation: true,
        },
      });
      attached = { cdp, authenticatorId };
      return attached;
    });
    if (attached) {
      await attached.cdp
        .send('WebAuthn.removeVirtualAuthenticator', { authenticatorId: attached.authenticatorId })
        .catch(() => {});
      await attached.cdp.send('WebAuthn.disable').catch(() => {});
    }
  },
  mockApi: async ({ page }, use) => {
    await use(async (log, routes) => {
      await page.route('**/api/**', async (route) => {
        const req = route.request();
        const path = new URL(req.url()).pathname;
        const h = routes[`${req.method()} ${path}`];
        if (!h) {
          await route.fulfill({ status: 404, body: `unmocked ${req.method()} ${path}` });
          return;
        }
        const raw = req.postData();
        const body = raw ? JSON.parse(raw) : undefined;
        log.push({ url: `${req.method()} ${path}`, body });
        const r = h(body);
        await route.fulfill({
          status: r.status ?? 200,
          contentType: 'application/json',
          body: JSON.stringify(r.json ?? {}),
        });
      });
    });
  },
});

const creationOptions = {
  publicKey: {
    rp: { id: RP_ID, name: 'Kanade' },
    user: { id: b64url('user-handle-1'), name: 'alice', displayName: 'alice' },
    challenge: b64url('registration-challenge-0123456789'),
    pubKeyCredParams: [{ type: 'public-key', alg: -7 }],
    timeout: 60000,
    attestation: 'none',
    authenticatorSelection: {
      residentKey: 'required',
      requireResidentKey: true,
      userVerification: 'required',
    },
  },
};

const requestOptions = {
  publicKey: {
    challenge: b64url('login-challenge-0123456789abcdef'),
    rpId: RP_ID,
    timeout: 60000,
    userVerification: 'required',
  },
};

const decodeClientData = (b64: string) => JSON.parse(Buffer.from(b64, 'base64url').toString());

test.describe('login screen', () => {
  test('no passkey UI at all when the backend has it switched off', async ({
    mount,
    mockApi,
  }) => {
    await mockApi([], { 'GET /api/auth/passkey/config': () => ({ json: { enabled: false } }) });
    const c = await mount(<LoginHost />);
    await expect(c.getByLabel('username')).toBeVisible();
    await expect(c.getByRole('button', { name: /passkey/i })).toHaveCount(0);
  });

  test('signs in with a passkey: user-verified browser output reaches the server intact', async ({
    mount,
    page,
    virtualAuthenticator,
    mockApi,
  }) => {
    await virtualAuthenticator();
    const origin = await page.evaluate(() => window.location.origin);
    // Mint a discoverable credential on the virtual device, as a prior
    // enrolment would have.
    await page.evaluate(
      async ({ opts }) => {
        const dec = (s: string) =>
          Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
        await navigator.credentials.create({
          publicKey: {
            ...opts.publicKey,
            challenge: dec(opts.publicKey.challenge),
            user: { ...opts.publicKey.user, id: dec(opts.publicKey.user.id) },
          } as PublicKeyCredentialCreationOptions,
        });
      },
      { opts: creationOptions },
    );

    const log: Captured[] = [];
    await mockApi(log, {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'POST /api/auth/passkey/login/start': () => ({
        json: { challenge_id: 'cid-1', options: requestOptions },
      }),
      'POST /api/auth/passkey/login/finish': () => ({
        json: { token: 'jwt-from-passkey', role: 'operator', must_change_pw: false, exp: 1 },
      }),
      'GET /api/auth/me': () => ({
        json: {
          username: 'alice',
          role: 'operator',
          must_change_pw: false,
          mfa_enabled: false,
          allowed_features: null,
        },
      }),
    });
    const c = await mount(<LoginHost />);
    await c.getByRole('button', { name: 'Sign in with a passkey' }).click();
    await expect(c.getByText('dashboard reached')).toBeVisible();

    const finish = log.find((l) => l.url === 'POST /api/auth/passkey/login/finish')!;
    expect(finish.body.challenge_id).toBe('cid-1');
    const cred = finish.body.credential;
    expect(cred.type).toBe('public-key');
    expect(cred.id).toBe(cred.rawId);
    for (const k of ['authenticatorData', 'clientDataJSON', 'signature', 'userHandle']) {
      expect(typeof cred.response[k], k).toBe('string');
      expect(cred.response[k]).not.toMatch(/[+/=]/);
    }
    // The browser, not our code, wrote the client data: it must bind to this
    // exact challenge and origin.
    const cd = decodeClientData(cred.response.clientDataJSON);
    expect(cd.type).toBe('webauthn.get');
    expect(cd.challenge).toBe(requestOptions.publicKey.challenge);
    expect(cd.origin).toBe(origin);
    // authenticatorData flags: UP (0x01) and UV (0x04) both set.
    const flags = Buffer.from(cred.response.authenticatorData, 'base64url')[32];
    expect(flags & 0x05).toBe(0x05);
    expect(await page.evaluate(() => localStorage.getItem('kanade_token'))).toBe('jwt-from-passkey');
  });

  test('a forced password change still gates a passkey session', async ({
    mount,
    page,
    virtualAuthenticator,
    mockApi,
  }) => {
    await virtualAuthenticator();
    await page.evaluate(
      async ({ opts }) => {
        const dec = (s: string) =>
          Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
        await navigator.credentials.create({
          publicKey: {
            ...opts.publicKey,
            challenge: dec(opts.publicKey.challenge),
            user: { ...opts.publicKey.user, id: dec(opts.publicKey.user.id) },
          } as PublicKeyCredentialCreationOptions,
        });
      },
      { opts: creationOptions },
    );
    await mockApi([], {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'POST /api/auth/passkey/login/start': () => ({
        json: { challenge_id: 'cid-2', options: requestOptions },
      }),
      'POST /api/auth/passkey/login/finish': () => ({
        json: { token: 't', role: 'viewer', must_change_pw: true, exp: 1 },
      }),
      'GET /api/auth/me': () => ({
        json: { username: 'a', role: 'viewer', must_change_pw: true, mfa_enabled: false, allowed_features: null },
      }),
    });
    const c = await mount(<LoginHost />);
    await c.getByRole('button', { name: 'Sign in with a passkey' }).click();
    await expect(c.getByText('change password gate')).toBeVisible();
  });

  test('dismissing the browser prompt is a quiet cancellation, not an error', async ({
    mount,
    virtualAuthenticator,
    mockApi,
  }) => {
    // No credential on the device => the browser rejects with NotAllowedError,
    // the same outcome as the user pressing Cancel.
    await virtualAuthenticator();
    const log: Captured[] = [];
    await mockApi(log, {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'POST /api/auth/passkey/login/start': () => ({
        json: { challenge_id: 'cid-3', options: requestOptions },
      }),
    });
    const c = await mount(<LoginHost />);
    await c.getByRole('button', { name: 'Sign in with a passkey' }).click();
    await expect(c.getByText(/Passkey sign-in was cancelled/)).toBeVisible();
    // Nothing was sent to the finish endpoint and the form is usable again.
    expect(log.some((l) => l.url.endsWith('login/finish'))).toBe(false);
    await expect(c.getByRole('button', { name: 'Sign in with a passkey' })).toBeEnabled();
    await expect(c.locator('.text-red-500')).toHaveCount(0);
  });

  test('a rejected assertion shows a generic failure and keeps the password form', async ({
    mount,
    page,
    virtualAuthenticator,
    mockApi,
  }) => {
    await virtualAuthenticator();
    await page.evaluate(
      async ({ opts }) => {
        const dec = (s: string) =>
          Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
        await navigator.credentials.create({
          publicKey: {
            ...opts.publicKey,
            challenge: dec(opts.publicKey.challenge),
            user: { ...opts.publicKey.user, id: dec(opts.publicKey.user.id) },
          } as PublicKeyCredentialCreationOptions,
        });
      },
      { opts: creationOptions },
    );
    await mockApi([], {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'POST /api/auth/passkey/login/start': () => ({
        json: { challenge_id: 'cid-4', options: requestOptions },
      }),
      'POST /api/auth/passkey/login/finish': () => ({ status: 401, json: 'passkey sign-in failed' }),
    });
    const c = await mount(<LoginHost />);
    await c.getByRole('button', { name: 'Sign in with a passkey' }).click();
    await expect(c.getByText(/Passkey sign-in failed/)).toBeVisible();
    await expect(c.getByLabel('password')).toBeVisible();
  });

  test('a browser without WebAuthn gets an explanation and a disabled button', async ({
    mount,
    page,
    mockApi,
  }) => {
    await page.evaluate(() => {
      // Simulate a browser/context with no WebAuthn.
      Object.defineProperty(window, 'PublicKeyCredential', { value: undefined, configurable: true });
    });
    await mockApi([], { 'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }) });
    const c = await mount(<LoginHost />);
    const btn = c.getByRole('button', { name: 'Sign in with a passkey' });
    await expect(btn).toBeDisabled();
    await expect(c.getByText(/Passkeys aren't available in this browser/)).toBeVisible();
    // The password path is untouched.
    await expect(c.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
  });
});

test.describe('account passkey card', () => {
  const me = (mfa: boolean) => ({
    username: 'alice',
    role: 'operator',
    must_change_pw: false,
    mfa_enabled: mfa,
    allowed_features: null,
  });

  const token = () => localStorage.setItem('kanade_token', 'jwt');

  test('adds a passkey: re-auth fields go to the server, the browser creates the credential', async ({
    mount,
    page,
    virtualAuthenticator,
    mockApi,
  }) => {
    await page.evaluate(token);
    const { cdp, authenticatorId } = await virtualAuthenticator();
    const origin = await page.evaluate(() => window.location.origin);
    let listed: unknown[] = [];
    const log: Captured[] = [];
    await mockApi(log, {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'GET /api/auth/me': () => ({ json: me(false) }),
      'GET /api/auth/passkeys': () => ({ json: listed }),
      'POST /api/auth/passkeys/register/start': () => ({
        json: { challenge_id: 'reg-1', options: creationOptions },
      }),
      'POST /api/auth/passkeys/register/finish': () => {
        listed = [{ id: 1, name: 'Work laptop', created_at: '2026-10-13 01:02:03', last_used_at: null }];
        return { status: 201, json: listed[0] };
      },
    });
    const c = await mount(<CardHost />);
    await expect(c.getByText('No passkeys registered yet.')).toBeVisible();
    await c.getByRole('button', { name: 'Add a passkey' }).click();
    await c.getByLabel('Name').fill('Work laptop');
    await c.getByLabel('Current password').fill('s3cret-pw');
    await c.getByRole('button', { name: 'Continue in browser' }).click();
    await expect(c.getByText('Work laptop')).toBeVisible();

    const start = log.find((l) => l.url === 'POST /api/auth/passkeys/register/start')!;
    expect(start.body).toEqual({ name: 'Work laptop', password: 's3cret-pw' });
    const finish = log.find((l) => l.url === 'POST /api/auth/passkeys/register/finish')!;
    expect(finish.body.challenge_id).toBe('reg-1');
    const cred = finish.body.credential;
    expect(cred.response.attestationObject).toMatch(/^[A-Za-z0-9_-]+$/);
    const cd = decodeClientData(cred.response.clientDataJSON);
    expect(cd.type).toBe('webauthn.create');
    expect(cd.challenge).toBe(creationOptions.publicKey.challenge);
    expect(cd.origin).toBe(origin);
    // The credential really landed on the device as a discoverable key.
    const { credentials } = await cdp.send('WebAuthn.getCredentials', { authenticatorId });
    expect(credentials).toHaveLength(1);
    expect(credentials[0].isResidentCredential).toBe(true);
    // The password is cleared from the form after use.
    await expect(c.getByLabel('Current password')).toHaveCount(0);
  });

  test('cancelling the browser prompt adds nothing and says so', async ({
    mount,
    page,
    virtualAuthenticator,
    mockApi,
  }) => {
    await page.evaluate(token);
    // Unverified device: the UV-required ceremony is refused, which the
    // browser reports exactly like a user pressing Cancel.
    await virtualAuthenticator({ verified: false });
    const log: Captured[] = [];
    await mockApi(log, {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'GET /api/auth/me': () => ({ json: me(false) }),
      'GET /api/auth/passkeys': () => ({ json: [] }),
      'POST /api/auth/passkeys/register/start': () => ({
        json: { challenge_id: 'reg-2', options: creationOptions },
      }),
    });
    const c = await mount(<CardHost />);
    await c.getByRole('button', { name: 'Add a passkey' }).click();
    await c.getByLabel('Name').fill('Phone');
    await c.getByLabel('Current password').fill('pw');
    await c.getByRole('button', { name: 'Continue in browser' }).click();
    await expect(c.getByText(/Passkey setup was cancelled/)).toBeVisible();
    expect(log.some((l) => l.url.endsWith('register/finish'))).toBe(false);
    await expect(c.getByRole('button', { name: 'Add a passkey' })).toBeVisible();
  });

  test('a TOTP account must supply a code, and is told why', async ({
    mount,
    page,
    virtualAuthenticator,
    mockApi,
  }) => {
    await page.evaluate(token);
    await virtualAuthenticator();
    const log: Captured[] = [];
    await mockApi(log, {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'GET /api/auth/me': () => ({ json: me(true) }),
      'GET /api/auth/passkeys': () => ({ json: [] }),
      'POST /api/auth/passkeys/register/start': () => ({
        json: { challenge_id: 'reg-3', options: creationOptions },
      }),
      'POST /api/auth/passkeys/register/finish': () => ({
        status: 201,
        json: { id: 2, name: 'k', created_at: '2026-10-13 01:02:03', last_used_at: null },
      }),
    });
    const c = await mount(<CardHost />);
    await expect(c.getByText(/does not ask for an authenticator code/)).toBeVisible();
    await c.getByRole('button', { name: 'Add a passkey' }).click();
    await c.getByLabel('Name').fill('k');
    await c.getByLabel('Current password').fill('pw');
    // No code yet -> cannot continue.
    await expect(c.getByRole('button', { name: 'Continue in browser' })).toBeDisabled();
    await c.getByLabel('Authenticator code').fill('123456');
    await c.getByRole('button', { name: 'Continue in browser' }).click();
    const start = log.find((l) => l.url === 'POST /api/auth/passkeys/register/start')!;
    expect(start.body).toEqual({ name: 'k', password: 'pw', totp_code: '123456' });
  });

  test('removal asks for the password and sends it', async ({ mount, page, mockApi }) => {
    await page.evaluate(token);
    const log: Captured[] = [];
    let rows = [
      { id: 7, name: 'Old tablet', created_at: '2026-01-02 03:04:05', last_used_at: '2026-02-03 04:05:06' },
    ];
    await mockApi(log, {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'GET /api/auth/me': () => ({ json: me(false) }),
      'GET /api/auth/passkeys': () => ({ json: rows }),
      'POST /api/auth/passkeys/7/remove': () => {
        rows = [];
        return { status: 204 };
      },
    });
    const c = await mount(<CardHost />);
    await expect(c.getByText('Old tablet')).toBeVisible();
    await c.getByRole('button', { name: 'Remove: Old tablet' }).click();
    await expect(c.getByRole('button', { name: 'Remove passkey' })).toBeDisabled();
    await c.getByLabel('Current password').fill('pw');
    await c.getByRole('button', { name: 'Remove passkey' }).click();
    await expect(c.getByText('No passkeys registered yet.')).toBeVisible();
    expect(log.find((l) => l.url.endsWith('/7/remove'))!.body).toEqual({ password: 'pw' });
  });

  test('an unsupported browser sees the explanation and cannot start enrolment', async ({
    mount,
    page,
    mockApi,
  }) => {
    await page.evaluate(token);
    await page.evaluate(() => {
      Object.defineProperty(window, 'PublicKeyCredential', { value: undefined, configurable: true });
    });
    await mockApi([], {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: true } }),
      'GET /api/auth/me': () => ({ json: me(false) }),
      'GET /api/auth/passkeys': () => ({ json: [] }),
    });
    const c = await mount(<CardHost />);
    await expect(c.getByText(/can't use passkeys/)).toBeVisible();
    await expect(c.getByRole('button', { name: 'Add a passkey' })).toBeDisabled();
  });

  test('the card is absent when passkeys are not configured', async ({
    mount,
    page,
    mockApi,
  }) => {
    await page.evaluate(token);
    await mockApi([], {
      'GET /api/auth/passkey/config': () => ({ json: { enabled: false } }),
      'GET /api/auth/me': () => ({ json: me(false) }),
    });
    const c = await mount(<CardHost />);
    await expect(c.getByText('Passkeys')).toHaveCount(0);
  });
});
