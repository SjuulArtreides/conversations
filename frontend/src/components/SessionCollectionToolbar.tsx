import { Input } from "./ui/input";
import { ProjectFilter } from "./ProjectFilter";
import { SortControl, type SortOrder } from "./SortControl";
import type { ProjectInfo } from "@/lib/types";
export function SessionCollectionToolbar({
  search,
  onSearch,
  project,
  onProject,
  projects,
  sort,
  onSort,
  sortOptions,
  searchLabel = "Search sessions or PRs",
  searchPlaceholder = "Search sessions or PRs…",
}: {
  search: string;
  onSearch: (value: string) => void;
  project: string;
  onProject: (value: string) => void;
  projects: ProjectInfo[];
  sort: SortOrder;
  onSort: (value: SortOrder) => void;
  sortOptions?: readonly SortOrder[];
  searchLabel?: string;
  searchPlaceholder?: string;
}) {
  return (
    <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
      <div className="relative flex-1">
        <svg
          className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          aria-hidden
        >
          <circle cx="11" cy="11" r="8" />
          <path d="m21 21-4.3-4.3" />
        </svg>
        <Input
          name="q"
          type="search"
          value={search}
          onChange={(e) => onSearch(e.target.value)}
          placeholder={searchPlaceholder}
          aria-label={searchLabel}
          className="pl-9"
        />
      </div>
      <ProjectFilter projects={projects} value={project} onChange={onProject} />
      <SortControl value={sort} onChange={onSort} options={sortOptions} />
    </div>
  );
}
