/**
 * Sanitize a server-supplied URL before it is used as an `<a href>`.
 *
 * The server validates ingested URLs (PR comment `html_url`, PR links) to be
 * absolute http(s), but the SPA renders them as links in several components —
 * defense in depth: anything that is not absolute http(s) (`javascript:`,
 * `data:`, relative paths) renders as a harmless in-page anchor instead of an
 * executable scheme.
 */
export function safeHref(url: string | undefined | null): string {
  if (!url) return "#";
  if (/^https?:\/\//i.test(url)) return url;
  return "#";
}
