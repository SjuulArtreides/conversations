import { exactTitleFirst } from "@/lib/sessionSearchRank";
import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useSearchParams } from "react-router-dom";
import { api } from "@/lib/api";
import { useDebounce } from "@/lib/useDebounce";
import { cn } from "@/lib/utils";
import { hostControl } from "@/lib/hostControl";
import { listSessions } from "@/lib/privateChannel";
import { privateSessionTitle } from "./PrivateSessionCard";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { Input } from "./ui/input";

interface SessionSearchDialogProps {
  hostId?: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

function basename(path: string | null): string {
  if (!path) return "";
  return path.split("/").filter(Boolean).at(-1) ?? "";
}

/**
 * Quick session switcher for the live-terminal view. It uses the same server
 * search as the sessions overview, so titles, transcript text, project paths,
 * uploaders, and PR references behave consistently in both places.
 */
export function SessionSearchDialog({
  open,
  onOpenChange,
  hostId,
}: SessionSearchDialogProps) {
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const [search, setSearch] = React.useState("");
  const [highlightedIndex, setHighlightedIndex] = React.useState(0);
  const debouncedSearch = useDebounce(search, 200);

  React.useEffect(() => {
    if (!open) {
      setSearch("");
      setHighlightedIndex(0);
    }
  }, [open]);

  const query = useQuery({
    queryKey: hostId
      ? ["private", hostId, "quick-search"]
      : ["sessions", "quick-search", debouncedSearch],
    queryFn: async () =>
      hostId
        ? (await hostControl.privateRead(hostId, listSessions)).map(
            (entry) => ({
              id: entry.id,
              title: privateSessionTitle(entry),
              project_path: entry.cwd,
              uploaded_by: "",
              is_live: entry.running,
            }),
          )
        : api.listSessions({ q: debouncedSearch }),
    staleTime: hostId ? 300_000 : 0,
    enabled: open,
    placeholderData: (previousData) => previousData,
  });

  const terms = debouncedSearch
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  const matches = (query.data ?? []).filter(
    (entry) =>
      !hostId ||
      terms.every((term) =>
        `${entry.title} ${entry.project_path}`.toLowerCase().includes(term),
      ),
  );
  const results = exactTitleFirst(
    matches,
    debouncedSearch,
    (entry) => entry.title,
  ).slice(0, 40);
  const resultsReady =
    search.trim() === debouncedSearch.trim() &&
    !query.isPlaceholderData &&
    !query.isFetching &&
    !query.isError;

  React.useEffect(() => {
    setHighlightedIndex((current) =>
      Math.min(current, Math.max(results.length - 1, 0)),
    );
  }, [results.length]);

  function openSession(session: { id: string; is_live?: boolean }) {
    const keepFullscreen =
      session.is_live === true && searchParams.get("fullscreen") === "1";
    onOpenChange(false);
    navigate(
      `/${hostId ? "private" : "sessions"}/${encodeURIComponent(session.id)}${keepFullscreen ? "?fullscreen=1" : ""}`,
    );
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Search sessions</DialogTitle>
          <DialogDescription>
            {hostId
              ? "Search session titles and project folders on your host."
              : "Search titles, transcripts, projects, uploaders, and PR references."}
          </DialogDescription>
        </DialogHeader>

        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!resultsReady) return;
            const selected = results[highlightedIndex] ?? results[0];
            if (selected) openSession(selected);
          }}
        >
          <div className="relative">
            <svg
              aria-hidden
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground"
            >
              <circle cx="11" cy="11" r="8" />
              <path d="m21 21-4.3-4.3" />
            </svg>
            <Input
              autoFocus
              type="search"
              value={search}
              onChange={(event) => {
                setSearch(event.target.value);
                setHighlightedIndex(0);
              }}
              onKeyDown={(event) => {
                if (!resultsReady || results.length === 0) return;
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  setHighlightedIndex((current) =>
                    Math.min(current + 1, results.length - 1),
                  );
                } else if (event.key === "ArrowUp") {
                  event.preventDefault();
                  setHighlightedIndex((current) => Math.max(current - 1, 0));
                }
              }}
              placeholder="Search sessions or PRs…"
              aria-label="Search sessions or PRs"
              className="pl-9"
            />
          </div>
        </form>

        <div className="mt-3 max-h-[min(28rem,60vh)] overflow-y-auto rounded-lg border border-border">
          {!resultsReady && !query.isError ? (
            <p className="px-3 py-6 text-center text-sm text-muted-foreground">
              Searching sessions…
            </p>
          ) : query.isError ? (
            <p className="px-3 py-6 text-center text-sm text-red-400">
              Failed to search sessions.
            </p>
          ) : results.length === 0 ? (
            <p className="px-3 py-6 text-center text-sm text-muted-foreground">
              No matching sessions.
            </p>
          ) : (
            <ul className="divide-y divide-border" aria-label="Session results">
              {results.map((session, index) => {
                const project = basename(session.project_path);
                const highlighted = index === highlightedIndex;
                return (
                  <li key={session.id}>
                    <button
                      type="button"
                      onClick={() => openSession(session)}
                      onFocus={() => setHighlightedIndex(index)}
                      onMouseEnter={() => setHighlightedIndex(index)}
                      className={cn(
                        "flex w-full items-center gap-3 px-3 py-2.5 text-left transition-colors hover:bg-accent focus-visible:bg-accent focus-visible:outline-hidden",
                        highlighted && "bg-accent",
                      )}
                      aria-label={`Open ${session.title}`}
                      aria-current={highlighted ? "true" : undefined}
                    >
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-sm font-medium text-foreground">
                          {session.title}
                        </span>
                        <span className="mt-0.5 block truncate text-xs text-muted-foreground">
                          {project ||
                            session.project_path ||
                            session.uploaded_by}
                        </span>
                      </span>
                      {session.is_live && (
                        <span className="shrink-0 rounded-full bg-red-500/15 px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-red-400">
                          Live
                        </span>
                      )}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>

        <p className="sr-only" role="status" aria-live="polite" aria-atomic>
          {resultsReady && results[highlightedIndex]
            ? `Selected ${results[highlightedIndex].title}, ${highlightedIndex + 1} of ${results.length}.`
            : ""}
        </p>

        {resultsReady && results.length > 0 && (
          <p className="mt-2 text-xs text-muted-foreground">
            Use ↑/↓ to choose, then Enter to open.
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}
