/** Keep the enrollment fragment on this device while an account signs in. */
const KEY = "conversations.pending-host-link";
export function saveHostLinkForLogin(hash: string) {
  if (!hash.startsWith("#desc=") || hash.length > 8192) return;
  sessionStorage.setItem(KEY, JSON.stringify({ hash, at: Date.now() }));
}
export function takeHostLinkAfterLogin(): string | null {
  const raw = sessionStorage.getItem(KEY);
  sessionStorage.removeItem(KEY);
  try {
    const value = JSON.parse(raw ?? "null") as {
      hash?: unknown;
      at?: unknown;
    } | null;
    if (
      typeof value?.hash !== "string" ||
      !value.hash.startsWith("#desc=") ||
      value.hash.length > 8192 ||
      typeof value.at !== "number" ||
      Date.now() - value.at > 600_000 ||
      value.at > Date.now()
    )
      return null;
    return value.hash;
  } catch {
    return null;
  }
}
