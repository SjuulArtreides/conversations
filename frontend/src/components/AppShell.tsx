import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { useLocalHost } from "@/lib/localHost";
import { AUTH_QUERY_KEY, useAuth } from "@/lib/useAuth";
import { suspendTerminalCache } from "@/lib/terminalCache";
import { Button } from "@/components/ui/button";
import { useToast } from "@/components/ui/toast";

import { Notifications } from "./Notifications";
import { SettingsGearLink } from "./SettingsGearLink";
import { HomeLink } from "./HomeLink";

const APP_INFO_QUERY_KEY = ["app-info"] as const;

function useSignOut() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { toast } = useToast();

  return async () => {
    try {
      await api.logout();
      await suspendTerminalCache();
      await queryClient.invalidateQueries({ queryKey: AUTH_QUERY_KEY });
      navigate("/login", { replace: true });
    } catch (error) {
      const message = error instanceof Error ? error.message : "Unknown error";
      toast(`Could not sign out: ${message}`, "error");
    }
  };
}

function VersionBadge() {
  const host = useLocalHost();
  const { data } = useQuery({
    queryKey: APP_INFO_QUERY_KEY,
    queryFn: api.appInfo,
    enabled: !host.local || host.authenticated,
    staleTime: 300_000,
    retry: false,
  });

  if (!data?.version) return null;

  return (
    <span
      className="hidden shrink-0 text-xs font-medium text-muted-foreground sm:inline"
      title={`Conversations ${data.version}`}
      aria-label={`Conversations version ${data.version}`}
    >
      v{data.version}
    </span>
  );
}

function AuthCorner() {
  const { authEnabled, user } = useAuth();
  const signOut = useSignOut();

  // Auth disabled server-side: no login UI at all.
  if (!authEnabled) return null;

  if (!user) {
    return (
      <Link
        to="/login"
        className="text-sm font-medium text-muted-foreground transition-colors hover:text-foreground"
      >
        Sign in
      </Link>
    );
  }

  return (
    <div className="flex items-center gap-3">
      <span
        className="hidden max-w-48 truncate text-sm text-muted-foreground sm:inline"
        title={user.email}
      >
        {user.display_name || user.email}
      </span>
      <Button variant="outline" size="sm" onClick={signOut}>
        Sign out
      </Button>
    </div>
  );
}

function MobileNavigation() {
  const { authEnabled, user } = useAuth();
  const signOut = useSignOut();

  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <Button
          variant="outline"
          size="sm"
          className="h-9 w-9 p-0 md:hidden"
          aria-label="Open navigation menu"
        >
          <svg
            className="h-4 w-4"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            aria-hidden
          >
            <path d="M4 6h16M4 12h16M4 18h16" />
          </svg>
        </Button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={8}
          aria-label="Navigation menu"
          className="z-50 w-56 rounded-lg border border-border bg-card p-1 shadow-xl animate-fade-in"
        >
          {user && (
            <>
              <div className="truncate px-2.5 py-2 text-xs text-muted-foreground">
                {user.display_name || user.email}
              </div>
              <DropdownMenu.Separator className="my-1 h-px bg-border" />
            </>
          )}
          <DropdownMenu.Item asChild>
            <Link
              to="/"
              className="block cursor-pointer rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
            >
              Sessions
            </Link>
          </DropdownMenu.Item>
          <DropdownMenu.Item asChild>
            <Link
              to="/projects"
              className="block cursor-pointer rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
            >
              Projects
            </Link>
          </DropdownMenu.Item>
          <DropdownMenu.Item asChild>
            <Link
              to="/private"
              className="block cursor-pointer rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
            >
              Private
            </Link>
          </DropdownMenu.Item>
          {authEnabled && (
            <>
              <DropdownMenu.Separator className="my-1 h-px bg-border" />
              {user ? (
                <DropdownMenu.Item
                  onSelect={() => void signOut()}
                  className="cursor-pointer rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
                >
                  Sign out
                </DropdownMenu.Item>
              ) : (
                <DropdownMenu.Item asChild>
                  <Link
                    to="/login"
                    className="block cursor-pointer rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
                  >
                    Sign in
                  </Link>
                </DropdownMenu.Item>
              )}
            </>
          )}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

export function AppShell({ children }: { children: React.ReactNode }) {
  const { pathname } = useLocation();
  const sessionPage =
    pathname.startsWith("/sessions/") || pathname.startsWith("/private/");
  const widthClass = sessionPage ? "max-w-[112rem]" : "max-w-5xl";

  return (
    <div className="min-h-full">
      <header className="sticky top-0 z-40 border-b border-border bg-background/80 backdrop-blur-sm supports-backdrop-filter:bg-background/60">
        <div
          className={`mx-auto flex h-14 items-center gap-3 px-4 ${widthClass}`}
        >
          <Link
            to="/"
            className="flex items-center gap-2 font-semibold tracking-tight transition-opacity hover:opacity-80"
          >
            <span className="grid h-7 w-7 place-items-center rounded-lg bg-primary/15 text-primary">
              💬
            </span>
            <span>Conversations</span>
          </Link>
          <VersionBadge />
          <nav className="hidden items-center gap-4 text-sm md:flex">
            <Link
              to="/"
              className="font-medium text-muted-foreground transition-colors hover:text-foreground"
            >
              Sessions
            </Link>
            <Link
              to="/projects"
              className="font-medium text-muted-foreground transition-colors hover:text-foreground"
            >
              Projects
            </Link>
            <Link
              to="/private"
              className="font-medium text-muted-foreground transition-colors hover:text-foreground"
            >
              Private
            </Link>
          </nav>
          <div className="ml-auto flex items-center gap-2">
            <Notifications />
          </div>
          <div className="hidden md:block">
            <AuthCorner />
          </div>
          <div className="md:hidden">
            <MobileNavigation />
          </div>
          {/* Settings is the rightmost control on every width — kept outside
              both the desktop AuthCorner and the mobile hamburger so it sits
              at the far right of the header, not next to the bell. */}
          <HomeLink />
          <SettingsGearLink />
        </div>
      </header>
      <main
        className={`mx-auto ${sessionPage ? "py-2 pl-1 pr-0 sm:px-4 sm:py-6" : "px-4 py-6"} ${widthClass}`}
      >
        {children}
      </main>
    </div>
  );
}
