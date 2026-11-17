import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { useDebounce } from "@/lib/useDebounce";
import { Input } from "./ui/input";

const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

interface EmailAutocompleteProps {
  selected: string[];
  onAdd: (email: string) => void;
  onRemove: (email: string) => void;
}

/** A chip list of selected emails plus a typeahead querying /api/users. */
export function EmailAutocomplete({
  selected,
  onAdd,
  onRemove,
}: EmailAutocompleteProps) {
  const [query, setQuery] = React.useState("");
  const [open, setOpen] = React.useState(false);
  const debounced = useDebounce(query, 300);

  const { data: users = [] } = useQuery({
    queryKey: ["users", debounced],
    queryFn: () => api.searchUsers(debounced),
    enabled: debounced.trim().length > 0,
  });

  const suggestions = users.filter((u) => !selected.includes(u.email));
  const typed = query.trim().toLowerCase();
  const canAddTyped =
    EMAIL_RE.test(typed) &&
    !selected.includes(typed) &&
    !suggestions.some((u) => u.email.toLowerCase() === typed);

  const add = (email: string) => {
    const e = email.trim().toLowerCase();
    if (!e || selected.includes(e)) return;
    onAdd(e);
    setQuery("");
    setOpen(false);
  };

  return (
    <div className="space-y-2">
      {selected.length > 0 && (
        <ul className="flex flex-wrap gap-1.5" aria-label="Shared with">
          {selected.map((email) => (
            <li
              key={email}
              className="inline-flex items-center gap-1 rounded-full border border-border bg-accent px-2.5 py-1 text-xs"
            >
              <span>{email}</span>
              <button
                type="button"
                onClick={() => onRemove(email)}
                aria-label={`Remove ${email}`}
                className="rounded-full text-muted-foreground transition-colors hover:text-destructive"
              >
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round">
                  <path d="M18 6 6 18M6 6l12 12" />
                </svg>
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="relative">
        <Input
          type="email"
          value={query}
          placeholder="Add people by email…"
          aria-label="Add people by email"
          autoComplete="off"
          onChange={(e) => {
            setQuery(e.target.value);
            setOpen(true);
          }}
          onFocus={() => setOpen(true)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              if (suggestions.length) add(suggestions[0].email);
              else if (canAddTyped) add(typed);
            } else if (e.key === "Escape") {
              setOpen(false);
            }
          }}
        />

        {open && (suggestions.length > 0 || canAddTyped) && (
          <ul className="absolute z-10 mt-1 max-h-56 w-full overflow-y-auto rounded-lg border border-border bg-card p-1 shadow-xl animate-fade-in">
            {suggestions.map((u) => (
              <li key={u.email}>
                <button
                  type="button"
                  onClick={() => add(u.email)}
                  className="flex w-full flex-col items-start rounded-md px-2.5 py-1.5 text-left text-sm transition-colors hover:bg-accent"
                >
                  <span>{u.email}</span>
                  {u.display_name && (
                    <span className="text-xs text-muted-foreground">
                      {u.display_name}
                    </span>
                  )}
                </button>
              </li>
            ))}
            {canAddTyped && (
              <li>
                <button
                  type="button"
                  onClick={() => add(typed)}
                  className="flex w-full items-center gap-1 rounded-md px-2.5 py-1.5 text-left text-sm transition-colors hover:bg-accent"
                >
                  Add <span className="font-medium">{typed}</span>
                </button>
              </li>
            )}
          </ul>
        )}
      </div>
    </div>
  );
}
