/** Stable relevance tier: literal, trimmed case-insensitive title equality wins. */
export function exactTitleFirst<T>(
  items: readonly T[],
  query: string,
  title: (item: T) => string | null | undefined,
): T[] {
  const normalized = query.trim().toLowerCase();
  if (!normalized) return [...items];
  const exact: T[] = [],
    partial: T[] = [];
  for (const item of items)
    ((title(item) ?? "").trim().toLowerCase() === normalized
      ? exact
      : partial
    ).push(item);
  return [...exact, ...partial];
}
