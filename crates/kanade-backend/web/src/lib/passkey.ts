/**
 * Browser side of passkey (WebAuthn) support: base64url <-> ArrayBuffer
 * plumbing, the server-JSON <-> `navigator.credentials` option mapping, and
 * the error classification the UI branches on.
 *
 * No library on purpose. The wire format is the one webauthn-rs speaks
 * (camelCase WebAuthn dictionaries with every binary field base64url, no
 * padding); `PublicKeyCredential.parse*FromJSON` / `.toJSON()` would do the
 * same job but are not available in every browser we serve, and these few
 * lines are the entire surface.
 */

/** base64url (RFC 4648 §5, unpadded) -> bytes. Throws on malformed input. */
export function b64urlToBytes(s: string): Uint8Array<ArrayBuffer> {
  const b64 = s.replace(/-/g, '+').replace(/_/g, '/');
  const bin = atob(b64 + '='.repeat((4 - (b64.length % 4)) % 4));
  const out = new Uint8Array(new ArrayBuffer(bin.length));
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** bytes -> base64url (unpadded). */
export function bytesToB64url(buf: ArrayBuffer | Uint8Array): string {
  const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/**
 * True when this browser can run a passkey ceremony at all. Needs a secure
 * context (HTTPS or localhost) as well as the API itself; either missing is
 * "unsupported" as far as the user is concerned.
 */
export function passkeysSupported(): boolean {
  return (
    typeof window !== 'undefined' &&
    window.isSecureContext !== false &&
    typeof window.PublicKeyCredential === 'function' &&
    typeof navigator !== 'undefined' &&
    !!navigator.credentials
  );
}

/**
 * The user dismissed the browser/OS prompt (or it timed out). WebAuthn
 * deliberately reports both as `NotAllowedError` so a site cannot tell
 * "declined" from "no such credential"; `AbortError` is our own abort.
 * Neither is a failure worth an error toast.
 */
export function isCancellation(err: unknown): boolean {
  return (
    typeof err === 'object' &&
    err !== null &&
    'name' in err &&
    ((err as { name: unknown }).name === 'NotAllowedError' ||
      (err as { name: unknown }).name === 'AbortError')
  );
}

type Json = Record<string, unknown>;

type DescriptorJson = { id: string } & Json;

function mapDescriptors(list: unknown): PublicKeyCredentialDescriptor[] | undefined {
  if (!Array.isArray(list)) return undefined;
  return (list as DescriptorJson[]).map((d) => ({
    ...(d as unknown as PublicKeyCredentialDescriptor),
    id: b64urlToBytes(d.id),
  }));
}

/** Server `publicKey` creation options (JSON) -> `navigator.credentials.create` input. */
export function toCreationOptions(json: {
  publicKey: Json;
}): CredentialCreationOptions {
  const pk = json.publicKey as Json & { user: Json & { id: string }; challenge: string };
  return {
    publicKey: {
      ...(pk as unknown as PublicKeyCredentialCreationOptions),
      challenge: b64urlToBytes(pk.challenge),
      user: { ...(pk.user as unknown as PublicKeyCredentialUserEntity), id: b64urlToBytes(pk.user.id) },
      excludeCredentials: mapDescriptors(pk.excludeCredentials),
    },
  };
}

/** Server `publicKey` request options (JSON) -> `navigator.credentials.get` input. */
export function toRequestOptions(json: { publicKey: Json }): CredentialRequestOptions {
  const pk = json.publicKey as Json & { challenge: string };
  return {
    publicKey: {
      ...(pk as unknown as PublicKeyCredentialRequestOptions),
      challenge: b64urlToBytes(pk.challenge),
      allowCredentials: mapDescriptors(pk.allowCredentials),
    },
    // Deliberately no `mediation`: this is an explicit button press, which
    // wants the modal account chooser rather than autofill UI.
  };
}

/** A fresh attestation -> the JSON body webauthn-rs parses. */
export function registrationToJSON(cred: PublicKeyCredential): Json {
  const r = cred.response as AuthenticatorAttestationResponse;
  return {
    id: cred.id,
    rawId: bytesToB64url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: bytesToB64url(r.attestationObject),
      clientDataJSON: bytesToB64url(r.clientDataJSON),
    },
    // Client extension results are unsigned and unused server-side.
    extensions: {},
  };
}

/** An assertion -> the JSON body webauthn-rs parses. */
export function assertionToJSON(cred: PublicKeyCredential): Json {
  const r = cred.response as AuthenticatorAssertionResponse;
  return {
    id: cred.id,
    rawId: bytesToB64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: bytesToB64url(r.authenticatorData),
      clientDataJSON: bytesToB64url(r.clientDataJSON),
      signature: bytesToB64url(r.signature),
      userHandle: r.userHandle ? bytesToB64url(r.userHandle) : null,
    },
    extensions: {},
  };
}

/** Run the browser's create ceremony for server options; returns the JSON to POST back. */
export async function createPasskey(options: { publicKey: Json }): Promise<Json> {
  const cred = await navigator.credentials.create(toCreationOptions(options));
  if (!cred) throw new DOMException('No credential returned', 'NotAllowedError');
  return registrationToJSON(cred as PublicKeyCredential);
}

/** Run the browser's get ceremony for server options; returns the JSON to POST back. */
export async function getPasskey(options: { publicKey: Json }): Promise<Json> {
  const cred = await navigator.credentials.get(toRequestOptions(options));
  if (!cred) throw new DOMException('No credential returned', 'NotAllowedError');
  return assertionToJSON(cred as PublicKeyCredential);
}
