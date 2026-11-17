import { fromBase64Url, toBase64Url } from "./hostAuth";

/**
 * Conversion between the host's WebAuthn JSON (webauthn-rs wire format) and
 * the browser's `navigator.credentials` objects. This is plumbing only: the
 * host verifies the ceremony, so nothing here decides whether a passkey is
 * valid.
 */

type Json = Record<string, unknown>;

const buf = (value: unknown): ArrayBuffer => {
  const bytes = fromBase64Url(String(value));
  return bytes.buffer.slice(
    bytes.byteOffset,
    bytes.byteOffset + bytes.byteLength,
  ) as ArrayBuffer;
};
const enc = (value: ArrayBuffer | null | undefined) =>
  value ? toBase64Url(new Uint8Array(value)) : undefined;

function descriptors(
  list: unknown,
): PublicKeyCredentialDescriptor[] | undefined {
  if (!Array.isArray(list)) return undefined;
  return list.map(
    (d: Json) => ({ ...d, id: buf(d.id) }) as PublicKeyCredentialDescriptor,
  );
}

function publicKey(options: unknown): Json {
  const inner = (options as { publicKey?: Json } | null)?.publicKey;
  if (!inner) throw new Error("The host sent no WebAuthn options.");
  return inner;
}

export function creationOptions(options: unknown): CredentialCreationOptions {
  const pk = publicKey(options);
  const user = pk.user as Json;
  return {
    publicKey: {
      ...pk,
      challenge: buf(pk.challenge),
      user: { ...user, id: buf(user.id) },
      excludeCredentials: descriptors(pk.excludeCredentials),
    } as unknown as PublicKeyCredentialCreationOptions,
  };
}

export function requestOptions(options: unknown): CredentialRequestOptions {
  const pk = publicKey(options);
  return {
    publicKey: {
      ...pk,
      challenge: buf(pk.challenge),
      allowCredentials: descriptors(pk.allowCredentials),
    } as unknown as PublicKeyCredentialRequestOptions,
  };
}

function ensureWebAuthn() {
  if (!navigator.credentials || typeof PublicKeyCredential === "undefined") {
    throw new Error(
      "This browser or page cannot use passkeys (they need a secure https page).",
    );
  }
}

/** Run the registration ceremony and return the host-shaped response JSON. */
export async function createPasskey(options: unknown): Promise<Json> {
  ensureWebAuthn();
  const credential = (await navigator.credentials.create(
    creationOptions(options),
  )) as PublicKeyCredential | null;
  if (!credential) throw new Error("No passkey was created.");
  const response = credential.response as AuthenticatorAttestationResponse;
  return {
    id: credential.id,
    rawId: enc(credential.rawId),
    type: credential.type,
    response: {
      attestationObject: enc(response.attestationObject),
      clientDataJSON: enc(response.clientDataJSON),
    },
    extensions: credential.getClientExtensionResults(),
  };
}

/** Run the assertion ceremony and return the host-shaped response JSON. */
export async function getPasskeyAssertion(options: unknown): Promise<Json> {
  ensureWebAuthn();
  const credential = (await navigator.credentials.get(
    requestOptions(options),
  )) as PublicKeyCredential | null;
  if (!credential) throw new Error("No passkey was used.");
  const response = credential.response as AuthenticatorAssertionResponse;
  return {
    id: credential.id,
    rawId: enc(credential.rawId),
    type: credential.type,
    response: {
      authenticatorData: enc(response.authenticatorData),
      clientDataJSON: enc(response.clientDataJSON),
      signature: enc(response.signature),
      userHandle: enc(response.userHandle),
    },
    extensions: credential.getClientExtensionResults(),
  };
}
