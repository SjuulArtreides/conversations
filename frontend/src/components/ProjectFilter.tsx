import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import type { ProjectInfo } from "@/lib/types";
import { shortProject } from "@/lib/utils";
import { Button } from "./ui/button";
import { Chevron } from "./ui/chevron";

interface ProjectFilterProps {
  projects: ProjectInfo[];
  value: string;
  onChange: (value: string) => void;
}

export function ProjectFilter({
  projects,
  value,
  onChange,
}: ProjectFilterProps) {
  const label = value ? shortProject(value) : "All projects";

  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <Button variant="secondary" className="min-w-48 justify-between">
          <span className="truncate" title={value || "All projects"}>
            {label}
          </span>
          <Chevron />
        </Button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={6}
          className="z-50 max-h-80 w-72 overflow-y-auto rounded-lg border border-border bg-card p-1 shadow-xl animate-fade-in"
        >
          <DropdownMenu.Item
            onSelect={() => onChange("")}
            className="flex cursor-pointer items-center justify-between rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
          >
            <span>All projects</span>
            {!value && <span className="text-primary">✓</span>}
          </DropdownMenu.Item>
          {projects.length > 0 && (
            <DropdownMenu.Separator className="my-1 h-px bg-border" />
          )}
          {projects.map((p) => (
            <DropdownMenu.Item
              key={p.project_path}
              onSelect={() => onChange(p.project_path)}
              className="flex cursor-pointer items-center justify-between gap-2 rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
            >
              <span className="truncate font-mono" title={p.project_path}>
                {shortProject(p.project_path)}
              </span>
              <span className="shrink-0 text-xs text-muted-foreground">
                {p.session_count}
              </span>
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
