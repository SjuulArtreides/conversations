import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/lib/api";
import {
  AUTH_QUERY_KEY,
  useAuth,
  useRedirectOnUnauthorized,
} from "@/lib/useAuth";
import { DeviceSetup } from "@/components/DeviceSetup";
import { LocalHostTrust } from "@/components/LocalHostTrust";
import { LOCAL_HOST_ID, localRequest, useLocalHost } from "@/lib/localHost";
import { BrowserKeyDirectory } from "@/components/BrowserKeyDirectory";
import { HostPairing } from "@/components/HostPairing";
import { HostSetup } from "@/components/HostSetup";
import { ConnectDevice } from "@/components/ConnectDevice";
import { HostNetworkSettings } from "@/components/HostNetworkSettings";
import { usePairedHosts } from "@/lib/hostPairing";
import { hostControl } from "@/lib/hostControl";
import type { HostLaunchSettings } from "@/lib/hostStatements";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { ErrorState, Spinner } from "@/components/states";
import { useToast } from "@/components/ui/toast";
import { DEFAULT_CODE_COLORS } from "@/lib/termClipboard";
import {
  MAX_PREFERRED_FONT_PX,
  MIN_PREFERRED_FONT_PX,
  usePreferredFontSize,
  writePreferredFontSize,
} from "@/lib/terminalFontSize";
import {
  ALL_LAUNCH_TARGETS,
  launchTargetKey,
  launchTargetLabel,
} from "@/lib/types";
import type {
  AdminDirectoryUser,
  AgentKind,
  AgentLauncher,
  CodexApprovalPolicy,
  CodexSandboxMode,
  InlineCodeColors,
  UserSettings,
} from "@/lib/types";

const SETTINGS_QUERY_KEY = ["user-settings"] as const;
/** Launch flags live on the host: read and written through signed operations. */
const HOST_LAUNCH_QUERY_KEY = ["host-launch-settings"] as const;
const ADMIN_USERS_QUERY_KEY = ["admin-users"] as const;
const CODEX_DEFAULT = "default" as const;
type CodexDefault = typeof CODEX_DEFAULT;
const SETTINGS_SECTIONS = [
  ["access", "Host access"],
  ["launch", "Launch"],
  ["appearance", "Appearance"],
  ["repositories", "Repositories"],
  ["administration", "Administration"],
] as const;
type SettingsSection = (typeof SETTINGS_SECTIONS)[number][0];

/** Format a packed `0xRRGGBB` value as a `#rrggbb` hex string for `<input>`. */
function numberToHex(n: number): string {
  return "#" + (n & 0xffffff).toString(16).padStart(6, "0");
}

/** Parse a `#rrggbb` hex string back to a packed `0xRRGGBB` number. */
function hexToNumber(hex: string): number {
  return parseInt(hex.replace(/^#/, ""), 16) & 0xffffff;
}

/** The baked-in Claude inline-code colour, as a hex string (single source). */
const DEFAULT_CODE_HEX = numberToHex([...DEFAULT_CODE_COLORS.rgb][0] ?? 0);

/** A top-level card for a settings group. */
function Card({
  children,
  hidden,
  "data-testid": testId,
}: {
  children: React.ReactNode;
  hidden?: boolean;
  "data-testid"?: string;
}) {
  return (
    <section
      data-testid={testId}
      hidden={hidden}
      className="rounded-xl border border-border bg-card"
    >
      {children}
    </section>
  );
}

/**
 * One label+controls row inside a grouped card. The label column is a fixed
 * width so every row in the card aligns; the controls column uses the rest
 * of the horizontal space, which stops narrow descriptions from stretching
 * tall when the controls would fit in one line.
 */
function Row({
  label,
  hint,
  children,
}: {
  label: React.ReactNode;
  hint?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="grid grid-cols-1 items-start gap-2 px-5 py-4 md:grid-cols-[minmax(0,10rem)_minmax(0,1fr)] md:gap-6 md:py-3.5">
      <div className="min-w-0 pt-1">
        <div className="text-sm font-medium text-foreground">{label}</div>
        {hint && (
          <div className="mt-0.5 text-xs text-muted-foreground">{hint}</div>
        )}
      </div>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

/**
 * The live terminal's preferred font size for this browser. Saved on blur or
 * Enter, so typing "1" on the way to "14" never applies an 8px font.
 */
function TerminalFontSizeRow() {
  const saved = usePreferredFontSize();
  const [draft, setDraft] = React.useState(saved === null ? "" : String(saved));
  React.useEffect(() => {
    setDraft(saved === null ? "" : String(saved));
  }, [saved]);

  const commit = () => {
    const trimmed = draft.trim();
    const value = Number(trimmed);
    if (trimmed === "") writePreferredFontSize(null);
    else if (Number.isFinite(value)) writePreferredFontSize(value);
    else setDraft(saved === null ? "" : String(saved));
  };

  return (
    <Row
      label="Terminal font size"
      hint={
        <>
          Live terminals render at this size, and{" "}
          <em>Fit columns and rows to screen</em> sizes the grid for it. Saved
          in this browser only, so each screen keeps its own size.
        </>
      }
    >
      <div className="flex flex-wrap items-center gap-3">
        <Input
          type="number"
          inputMode="numeric"
          aria-label="Terminal font size"
          min={MIN_PREFERRED_FONT_PX}
          max={MAX_PREFERRED_FONT_PX}
          step={1}
          placeholder="Default"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
          }}
          className="w-24"
        />
        <span className="text-xs text-muted-foreground">px</span>
        <Button
          size="sm"
          variant="ghost"
          aria-label="Reset terminal font size to default"
          onClick={() => writePreferredFontSize(null)}
          disabled={saved === null}
        >
          Reset to default
        </Button>
      </div>
    </Row>
  );
}

/** A radio option with room to explain what the choice does. */
function LaunchChoice({
  value,
  name,
  title,
  detail,
  disabled,
}: {
  value: string;
  name: string;
  title: string;
  detail: string;
  disabled?: boolean;
}) {
  const detailId = React.useId();
  return (
    <label className="flex min-w-0 cursor-pointer items-start gap-3 rounded-lg border border-border/70 bg-background/40 p-3 transition-colors hover:border-primary/50 [&:has([data-state=checked])]:border-primary/70 [&:has([data-state=checked])]:bg-primary/10 has-[:disabled]:cursor-not-allowed has-[:disabled]:opacity-50">
      <RadioGroupItem
        value={value}
        aria-label={name}
        aria-describedby={detailId}
        disabled={disabled}
        className="mt-0.5 shrink-0"
      />
      <span className="min-w-0">
        <span className="block text-sm font-medium text-foreground">
          {title}
        </span>
        <span
          id={detailId}
          className="mt-1 block text-xs leading-relaxed text-muted-foreground"
        >
          {detail}
        </span>
      </span>
    </label>
  );
}

export function Settings() {
  const queryClient = useQueryClient();
  const { toast } = useToast();
  const { isAdmin, passwordEnabled, user: currentUser } = useAuth();
  const host = useLocalHost();
  const [section, setSection] = React.useState<SettingsSection>("access");
  const [advancedOpen, setAdvancedOpen] = React.useState(false);
  const activeSection =
    section === "administration" && !isAdmin ? "access" : section;
  const settingsQueryKey = host.local
    ? (["local-user-settings"] as const)
    : SETTINGS_QUERY_KEY;
  // How this browser reaches the active paired host (its own settings).
  const pairing = usePairedHosts();
  const activeHostId = pairing.hosts.some((h) => h.id === pairing.active)
    ? pairing.active
    : null;
  const selectedHost = pairing.hosts.find(
    (candidate) => candidate.id === activeHostId,
  );
  const launchHostId = host.local ? LOCAL_HOST_ID : (activeHostId ?? undefined);
  const launchAuthMode = host.local
    ? "local"
    : selectedHost
      ? (selectedHost.authMode ?? "passkey")
      : "unpaired";
  const launchQueryKey = [
    ...HOST_LAUNCH_QUERY_KEY,
    launchHostId ?? null,
    launchAuthMode,
  ] as const;
  const launchIdentity = JSON.stringify(launchQueryKey);
  const [launchDraftIdentity, setLaunchDraftIdentity] = React.useState<
    string | null
  >(null);

  // Local editable copy of the container paths. Seeded from the loaded
  // settings once they arrive (and kept in sync if the server data changes).
  const [paths, setPaths] = React.useState<string[]>([]);
  const [draft, setDraft] = React.useState("");
  const [defaultLauncher, setDefaultLauncher] =
    React.useState<AgentLauncher>("opencode");
  const [defaultAgent, setDefaultAgent] = React.useState<AgentKind>("claude");
  const [codexSandboxMode, setCodexSandboxMode] = React.useState<
    CodexSandboxMode | CodexDefault
  >(CODEX_DEFAULT);
  const [codexApprovalPolicy, setCodexApprovalPolicy] = React.useState<
    CodexApprovalPolicy | CodexDefault
  >(CODEX_DEFAULT);
  const [claudeSkipPermissions, setClaudeSkipPermissions] =
    React.useState(false);
  const [opencodeAuto, setOpencodeAuto] = React.useState(false);
  const [adkStatusline, setAdkStatusline] = React.useState(false);
  // The panel being inspected is independent of the saved default agent.
  const [launchPanel, setLaunchPanel] = React.useState<
    "claude" | "codex" | "opencode"
  >("claude");
  const initialPanelSet = React.useRef(false);
  // Inline-code colour for Slack-friendly terminal copy, as a #rrggbb hex.
  // Seeded from the saved rgb[0], else the baked-in Claude default.
  const [codeColor, setCodeColor] = React.useState<string>(DEFAULT_CODE_HEX);
  const [resetLink, setResetLink] = React.useState<{
    email: string;
    url: string;
  } | null>(null);

  const { data, isLoading, isError, error, refetch } = useQuery({
    queryKey: settingsQueryKey,
    queryFn: () =>
      host.local
        ? localRequest<UserSettings>("/api/local/preferences")
        : api.getUserSettings(),
  });

  useRedirectOnUnauthorized(error);

  // The launch flags are the HOST's own (host-signed answer, local file). There
  // is no fallback to a central copy: without a paired, authorized host they
  // cannot be read or changed, and the card says so.
  const hostQuery = useQuery({
    queryKey: launchQueryKey,
    queryFn: () =>
      launchHostId
        ? hostControl.getHostSettings(launchHostId)
        : hostControl.getHostSettings(),
    retry: false,
    staleTime: 30_000,
  });
  const hostLaunch = hostQuery.isSuccess ? hostQuery.data : undefined;
  const launchLocked = !hostLaunch || launchDraftIdentity !== launchIdentity;

  const {
    data: adminUsers = [],
    isLoading: adminUsersLoading,
    isError: adminUsersError,
  } = useQuery({
    queryKey: ADMIN_USERS_QUERY_KEY,
    queryFn: api.listAdminUsers,
    enabled: isAdmin,
  });

  // Seed the editable fields from the loaded settings.
  React.useEffect(() => {
    if (data) {
      setPaths(data.container_paths);
      setDefaultAgent(data.default_agent ?? "claude");
      const savedRgb = data.inline_code_colors?.rgb?.[0];
      setCodeColor(savedRgb != null ? numberToHex(savedRgb) : DEFAULT_CODE_HEX);
    }
  }, [data]);

  React.useEffect(() => {
    if (hostLaunch) {
      setDefaultLauncher(hostLaunch.default_launcher);
      setCodexSandboxMode(hostLaunch.codex_sandbox_mode ?? CODEX_DEFAULT);
      setCodexApprovalPolicy(hostLaunch.codex_approval_policy ?? CODEX_DEFAULT);
      setOpencodeAuto(hostLaunch.opencode_auto);
      setClaudeSkipPermissions(hostLaunch.claude_skip_permissions);
      setAdkStatusline(hostLaunch.adk_statusline);
      setLaunchDraftIdentity(launchIdentity);
    }
  }, [hostLaunch, launchIdentity]);

  React.useEffect(() => {
    if (!initialPanelSet.current && data && !hostQuery.isLoading) {
      const agent = data.default_agent;
      if (agent === "claude" || agent === "codex" || agent === "opencode")
        setLaunchPanel(agent);
      initialPanelSet.current = true;
    }
  }, [data, hostQuery.isLoading]);

  const save = useMutation({
    mutationFn: (next: {
      paths: string[];
      defaultAgent: AgentKind;
      defaultLauncher: AgentLauncher;
      codexSandboxMode: CodexSandboxMode | CodexDefault;
      codexApprovalPolicy: CodexApprovalPolicy | CodexDefault;
      opencodeAuto: boolean;
      claudeSkipPermissions: boolean;
      adkStatusline: boolean;
      codeColor: string;
      launchSnapshot: {
        id?: string;
        key: readonly unknown[];
        settings?: HostLaunchSettings;
      };
    }) => {
      // Wholesale PUT — always send the colour so it isn't cleared. Stored as
      // a single-rgb config; the localStorage override covers multi/palette.
      const colors: InlineCodeColors = {
        palette: [],
        rgb: [hexToNumber(next.codeColor)],
      };
      // Account preferences only. Launch flags never go to the server.
      const preferences = host.local
        ? localRequest<UserSettings>("/api/local/preferences", {
            method: "PUT",
            body: JSON.stringify({
              container_paths: next.paths,
              default_agent: next.defaultAgent,
              inline_code_colors: colors,
            }),
          })
        : api.setUserSettings(next.paths, next.defaultAgent, colors);
      return preferences.then(async (saved) => {
        const snapshot = next.launchSnapshot;
        if (snapshot.settings) {
          const launch = {
            claude_skip_permissions: next.claudeSkipPermissions,
            opencode_auto: next.opencodeAuto,
            codex_sandbox_mode:
              next.codexSandboxMode === CODEX_DEFAULT
                ? null
                : next.codexSandboxMode,
            codex_approval_policy:
              next.codexApprovalPolicy === CODEX_DEFAULT
                ? null
                : next.codexApprovalPolicy,
            default_launcher: next.defaultLauncher,
            adk_statusline: next.adkStatusline,
          };
          if (JSON.stringify(launch) !== JSON.stringify(snapshot.settings)) {
            if (snapshot.id)
              await hostControl.updateHostSettings(launch, snapshot.id);
            else await hostControl.updateHostSettings(launch);
            queryClient.setQueryData(snapshot.key, launch);
          }
        }
        return saved;
      });
    },
    onSuccess: (saved) => {
      queryClient.setQueryData(settingsQueryKey, saved);
      setPaths(saved.container_paths);
      setDefaultAgent(saved.default_agent ?? "claude");
      const savedRgb = saved.inline_code_colors?.rgb?.[0];
      setCodeColor(savedRgb != null ? numberToHex(savedRgb) : DEFAULT_CODE_HEX);
      toast("Settings saved");
    },
    onError: (err: Error) =>
      toast(`Could not save settings: ${err.message}`, "error"),
  });

  const createResetLink = useMutation({
    mutationFn: (email: string) => api.createPasswordResetLink(email),
    onSuccess: (result, email) => {
      setResetLink({
        email,
        url: new URL(result.reset_path, window.location.origin).toString(),
      });
      toast("Password reset link created");
    },
    onError: (err: Error) =>
      toast(`Could not create reset link: ${err.message}`, "error"),
  });

  const grantAdmin = useMutation({
    mutationFn: (email: string) => api.grantAdmin(email),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ADMIN_USERS_QUERY_KEY });
      await queryClient.invalidateQueries({ queryKey: AUTH_QUERY_KEY });
      toast("Administrator access granted");
    },
    onError: (err: Error) =>
      toast(`Could not grant administrator access: ${err.message}`, "error"),
  });

  const revokeAdmin = useMutation({
    mutationFn: (email: string) => api.revokeAdmin(email),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ADMIN_USERS_QUERY_KEY });
      await queryClient.invalidateQueries({ queryKey: AUTH_QUERY_KEY });
      toast("Administrator access revoked");
    },
    onError: (err: Error) =>
      toast(`Could not revoke administrator access: ${err.message}`, "error"),
  });

  // The user the admin has chosen to delete, pending confirmation.
  const [pendingDelete, setPendingDelete] =
    React.useState<AdminDirectoryUser | null>(null);

  const deleteUser = useMutation({
    mutationFn: (email: string) => api.deleteUser(email),
    onSuccess: async (_, email) => {
      await queryClient.invalidateQueries({ queryKey: ADMIN_USERS_QUERY_KEY });
      await queryClient.invalidateQueries({ queryKey: AUTH_QUERY_KEY });
      setPendingDelete(null);
      toast(`Deleted ${email}`);
    },
    onError: (err: Error) =>
      toast(`Could not delete user: ${err.message}`, "error"),
  });

  function addPath() {
    const trimmed = draft.trim();
    if (!trimmed) return;
    setPaths((prev) => (prev.includes(trimmed) ? prev : [...prev, trimmed]));
    setDraft("");
  }

  function removePath(target: string) {
    setPaths((prev) => prev.filter((p) => p !== target));
  }

  const canSave = !isLoading && !isError;
  const savedRgb = data?.inline_code_colors?.rgb?.[0];
  const savedCodeColor =
    savedRgb != null ? numberToHex(savedRgb) : DEFAULT_CODE_HEX;
  const hasUnsavedChanges =
    !!data &&
    (JSON.stringify(paths) !== JSON.stringify(data.container_paths) ||
      defaultAgent !== (data.default_agent ?? "claude") ||
      codeColor.toLowerCase() !== savedCodeColor.toLowerCase() ||
      (!!hostLaunch &&
        (defaultLauncher !== hostLaunch.default_launcher ||
          codexSandboxMode !==
            (hostLaunch.codex_sandbox_mode ?? CODEX_DEFAULT) ||
          codexApprovalPolicy !==
            (hostLaunch.codex_approval_policy ?? CODEX_DEFAULT) ||
          opencodeAuto !== hostLaunch.opencode_auto ||
          claudeSkipPermissions !== hostLaunch.claude_skip_permissions ||
          adkStatusline !== hostLaunch.adk_statusline)));

  const saveButton = (
    <Button
      className={
        hasUnsavedChanges
          ? "bg-amber-400 text-slate-950 hover:bg-amber-300"
          : undefined
      }
      title={hasUnsavedChanges ? "Unsaved changes" : "Settings saved"}
      onClick={() =>
        save.mutate({
          paths,
          defaultAgent,
          defaultLauncher,
          codexSandboxMode,
          codexApprovalPolicy,
          opencodeAuto,
          claudeSkipPermissions,
          adkStatusline,
          codeColor,
          launchSnapshot: {
            id: launchHostId,
            key: launchQueryKey,
            settings: launchLocked ? undefined : hostLaunch,
          },
        })
      }
      disabled={save.isPending}
    >
      {save.isPending ? "Saving…" : "Save"}
    </Button>
  );

  return (
    <div className="space-y-4">
      {/* Page header: Save sits in the title row and the row sticks just below
          the AppShell header (which is itself sticky at top-0, 56–57 px tall)
          so Save stays visible on every scroll position without floating over
          form controls or pairing inputs. Hidden while the initial load is in
          flight or errored. */}
      <div className="sticky top-14 z-10 -mx-4 flex flex-wrap items-center justify-between gap-3 border-b border-border/60 bg-background/90 px-4 py-3 backdrop-blur">
        <h1 className="text-2xl font-semibold tracking-tight">Settings</h1>
        {canSave &&
          activeSection !== "access" &&
          activeSection !== "administration" &&
          saveButton}
      </div>

      <nav aria-label="Settings sections" className="flex flex-wrap gap-2">
        {SETTINGS_SECTIONS.filter(
          ([id]) => id !== "administration" || isAdmin,
        ).map(([id, label]) => (
          <Button
            key={id}
            variant={activeSection === id ? "secondary" : "ghost"}
            aria-current={activeSection === id ? "page" : undefined}
            onClick={() => setSection(id)}
          >
            {label}
          </Button>
        ))}
      </nav>

      <div hidden={activeSection !== "access"} className="space-y-4">
        {host.local ? (
          <>
            <Card>
              <div className="p-5">
                <DeviceSetup />
              </div>
            </Card>
            <Card>
              <div className="p-5">
                <LocalHostTrust />
              </div>
            </Card>
          </>
        ) : (
          <>
            <Card>
              <div className="p-5">
                <HostSetup active={activeSection === "access"} />
              </div>
            </Card>
            <details
              className="rounded-xl border border-border bg-card p-5"
              onToggle={(event) => setAdvancedOpen(event.currentTarget.open)}
            >
              <summary className="cursor-pointer text-sm font-medium">
                Advanced
              </summary>
              {advancedOpen && activeSection === "access" && (
                <div className="mt-5 space-y-5">
                  <ConnectDevice />
                  <BrowserKeyDirectory showPublication={false} />
                  <HostPairing />
                  {activeHostId && (
                    <HostNetworkSettings
                      key={activeHostId}
                      hostId={activeHostId}
                    />
                  )}
                </div>
              )}
            </details>
          </>
        )}
      </div>

      {isAdmin && (
        <Card hidden={activeSection !== "administration"}>
          <div className="border-b border-border/60 px-5 py-4">
            <h2 className="text-base font-semibold text-foreground">
              User management
            </h2>
            <p className="mt-1 text-sm text-muted-foreground">
              Grant or revoke administrator access
              {passwordEnabled && ", and create one-time password-reset links"}.
              Initial administrators are configured on the server and cannot be
              removed here.
            </p>
          </div>
          <div className="p-5">
            {adminUsersLoading && (
              <p className="text-sm text-muted-foreground">Loading users…</p>
            )}
            {adminUsersError && (
              <p className="text-sm text-destructive">Could not load users.</p>
            )}
            {!adminUsersLoading &&
              !adminUsersError &&
              adminUsers.length === 0 && (
                <p className="text-sm text-muted-foreground">No users found.</p>
              )}
            {adminUsers.length > 0 && (
              <ul className="space-y-2">
                {adminUsers.map((user) => {
                  const creatingResetLink =
                    createResetLink.isPending &&
                    createResetLink.variables === user.email;
                  const grantingAdmin =
                    grantAdmin.isPending && grantAdmin.variables === user.email;
                  const revokingAdmin =
                    revokeAdmin.isPending &&
                    revokeAdmin.variables === user.email;
                  const isSelf = currentUser?.email === user.email;
                  return (
                    <li
                      key={user.email}
                      className="rounded-lg border border-border/70 p-3"
                    >
                      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
                        <div className="min-w-0">
                          {user.display_name && (
                            <div className="truncate text-sm font-medium">
                              {user.display_name}
                            </div>
                          )}
                          <div className="truncate text-sm text-muted-foreground">
                            {user.email}
                          </div>
                          <div className="mt-1 text-xs text-muted-foreground">
                            {user.is_initial
                              ? "Initial administrator"
                              : user.is_admin
                                ? "Administrator"
                                : "User"}
                          </div>
                        </div>
                        <div className="flex flex-wrap gap-2">
                          {passwordEnabled && (
                            <Button
                              size="sm"
                              variant="outline"
                              aria-label={`Create reset link for ${user.email}`}
                              onClick={() => {
                                setResetLink(null);
                                createResetLink.mutate(user.email);
                              }}
                              disabled={creatingResetLink}
                            >
                              {creatingResetLink
                                ? "Creating…"
                                : "Create reset link"}
                            </Button>
                          )}
                          {!user.is_admin && (
                            <Button
                              size="sm"
                              variant="outline"
                              aria-label={`Make ${user.email} an administrator`}
                              onClick={() => grantAdmin.mutate(user.email)}
                              disabled={grantingAdmin}
                            >
                              {grantingAdmin ? "Granting…" : "Make admin"}
                            </Button>
                          )}
                          {user.is_admin && !user.is_initial && (
                            <Button
                              size="sm"
                              variant="ghost"
                              className="text-destructive hover:bg-destructive/10 hover:text-destructive"
                              aria-label={`Remove administrator access from ${user.email}`}
                              onClick={() => revokeAdmin.mutate(user.email)}
                              disabled={revokingAdmin}
                            >
                              {revokingAdmin ? "Removing…" : "Remove admin"}
                            </Button>
                          )}
                          {!user.is_initial && !isSelf && (
                            <Button
                              size="sm"
                              variant="ghost"
                              className="text-destructive hover:bg-destructive/10 hover:text-destructive"
                              aria-label={`Delete ${user.email}`}
                              onClick={() => setPendingDelete(user)}
                            >
                              Delete user
                            </Button>
                          )}
                        </div>
                      </div>
                      {passwordEnabled && resetLink?.email === user.email && (
                        <label className="mt-3 flex flex-col gap-1 text-sm">
                          <span className="font-medium">
                            One-time reset link
                          </span>
                          <Input
                            readOnly
                            value={resetLink.url}
                            aria-label={`One-time reset link for ${user.email}`}
                          />
                        </label>
                      )}
                    </li>
                  );
                })}
              </ul>
            )}
          </div>
        </Card>
      )}

      <Dialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open && !deleteUser.isPending) setPendingDelete(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete user?</DialogTitle>
            <DialogDescription>
              {pendingDelete && (
                <>
                  <span className="font-medium text-foreground">
                    {pendingDelete.display_name
                      ? `${pendingDelete.display_name} (${pendingDelete.email})`
                      : pendingDelete.email}
                  </span>{" "}
                  will be removed from the directory, together with their
                  password, administrator grant, saved preferences, trusted
                  browser keys, and any pending password-reset links. The
                  account will also be dropped from every share list it appears
                  on.
                </>
              )}
            </DialogDescription>
            <DialogDescription>
              Sessions, projects, and chats this user authored are kept as-is
              with their email preserved for audit history. Delete those
              separately if you need them gone too. This action cannot be
              undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="ghost"
              onClick={() => setPendingDelete(null)}
              disabled={deleteUser.isPending}
            >
              Cancel
            </Button>
            <Button
              variant="outline"
              className="border-destructive bg-destructive text-destructive-foreground hover:bg-destructive/90"
              aria-label={
                pendingDelete
                  ? `Confirm deleting ${pendingDelete.email}`
                  : undefined
              }
              onClick={() => {
                if (pendingDelete) deleteUser.mutate(pendingDelete.email);
              }}
              disabled={deleteUser.isPending || !pendingDelete}
            >
              {deleteUser.isPending ? "Deleting…" : "Delete user"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Launch — one card for everything that affects how a session starts. */}
      <Card
        hidden={activeSection !== "launch"}
        data-testid="host-launch-status"
      >
        <div className="border-b border-border/60 px-5 py-4">
          <div className="flex flex-wrap items-baseline justify-between gap-2">
            <h2 className="text-base font-semibold text-foreground">Launch</h2>
            <span className="text-xs text-muted-foreground">
              Applied to new launches and reopens
            </span>
          </div>
          <p className="mt-1 text-sm text-muted-foreground">
            How each coding agent is started. Launch flags are kept on your
            paired host and read with signed requests; the server keeps no copy.
          </p>
          <details className="mt-1 text-sm text-muted-foreground">
            <summary className="cursor-pointer text-foreground/70 hover:text-foreground">
              More about per-host launch flags
            </summary>
            <p className="mt-2">
              The flags on this card are stored on the paired host's local file
              and never on the server, so a different browser only sees them
              after it pairs with that host. Changes apply to the next launch or
              reopen — sessions already running are not affected.
            </p>
          </details>

          {hostQuery.isLoading && (
            <p className="mt-3 text-sm text-muted-foreground">
              Reading the host's settings…
            </p>
          )}
          {hostQuery.isError && (
            <p
              role="alert"
              className="mt-3 flex flex-wrap items-center gap-3 rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive"
            >
              <span className="flex-1">
                {(hostQuery.error as Error).message}
              </span>
              <Button
                size="sm"
                variant="outline"
                onClick={() => void hostQuery.refetch()}
              >
                Try again
              </Button>
            </p>
          )}
        </div>

        <div className="space-y-6 p-5 sm:p-6">
          <div>
            <div className="flex flex-wrap items-end justify-between gap-2">
              <div>
                <h3 className="text-sm font-semibold">
                  Default for new sessions
                </h3>
                <p className="mt-1 text-sm text-muted-foreground">
                  Choose what is preselected when you start a session.
                </p>
              </div>
            </div>
            <RadioGroup
              aria-label="Default agent"
              value={launchTargetKey({
                agent: defaultAgent,
                launcher: defaultLauncher,
              })}
              onValueChange={(value) => {
                const target = ALL_LAUNCH_TARGETS.find(
                  (item) => launchTargetKey(item) === value,
                );
                if (target) {
                  setDefaultAgent(target.agent);
                  setDefaultLauncher(target.launcher ?? "opencode");
                }
              }}
              className="mt-4 grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-5"
            >
              {ALL_LAUNCH_TARGETS.map((target) => {
                const key = launchTargetKey(target);
                const isAdk = target.launcher === "adk";
                const labels: Record<string, string> = {
                  claude: "Claude",
                  codex: "Codex",
                  opencode: "OpenCode",
                  "opencode:adk": "adk code",
                  zsh: "Zsh",
                };
                const details: Record<string, string> = {
                  claude: "Claude Code",
                  codex: "OpenAI Codex",
                  opencode: "Native launcher",
                  "opencode:adk": "Arrakis harness",
                  zsh: "Shell",
                };
                return (
                  <LaunchChoice
                    key={key}
                    value={key}
                    name={launchTargetLabel(target)}
                    title={labels[key]}
                    detail={details[key]}
                    disabled={isAdk && launchLocked}
                  />
                );
              })}
            </RadioGroup>
          </div>

          <div className="overflow-hidden rounded-xl border border-border/70 bg-background/30">
            <div className="border-b border-border/60 px-4 py-4 sm:px-5">
              <p className="text-xs font-semibold uppercase tracking-wider text-primary">
                Agent configuration
              </p>
              <h3 className="mt-1 text-base font-semibold">
                Make each launch yours
              </h3>
              <p className="mt-1 text-sm text-muted-foreground">
                Choose an agent to edit its settings. This does not change your
                default.
              </p>
            </div>
            <div
              className="flex flex-wrap gap-1 border-b border-border/60 px-3 pt-2"
              role="group"
              aria-label="Agent configuration"
            >
              {(
                [
                  ["claude", "Claude"],
                  ["codex", "Codex"],
                  ["opencode", "OpenCode & adk"],
                ] as const
              ).map(([key, label]) => (
                <button
                  key={key}
                  type="button"
                  aria-pressed={launchPanel === key}
                  onClick={() => {
                    initialPanelSet.current = true;
                    setLaunchPanel(key);
                  }}
                  className="-mb-px border-b-2 border-transparent px-3 py-2.5 text-sm font-medium text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring aria-pressed:border-primary aria-pressed:text-foreground"
                >
                  {label}
                </button>
              ))}
            </div>
            <div className="space-y-6 p-4 sm:p-5">
              {launchPanel === "claude" && (
                <div>
                  <h4 className="text-sm font-semibold">Permission prompts</h4>
                  <p className="mt-1 text-sm text-muted-foreground">
                    Choose how Claude handles permission requests at launch.
                  </p>
                  <RadioGroup
                    aria-label="Claude permissions"
                    disabled={launchLocked}
                    value={claudeSkipPermissions ? "skip" : "default"}
                    onValueChange={(value) =>
                      setClaudeSkipPermissions(value === "skip")
                    }
                    className="mt-3 grid gap-2 sm:grid-cols-2"
                  >
                    <LaunchChoice
                      value="default"
                      name="Use Claude settings"
                      title="Use Claude's settings"
                      detail="Keep the permissions configured on your host."
                    />
                    <LaunchChoice
                      value="skip"
                      name="Skip permission prompts"
                      title="Skip permission prompts"
                      detail="Launch with --dangerously-skip-permissions."
                    />
                  </RadioGroup>
                </div>
              )}
              {launchPanel === "codex" && (
                <>
                  <div>
                    <h4 className="text-sm font-semibold">Sandbox access</h4>
                    <p className="mt-1 text-sm text-muted-foreground">
                      Set what Codex can access when it runs commands.
                    </p>
                    <RadioGroup
                      aria-label="Codex sandbox"
                      disabled={launchLocked}
                      value={codexSandboxMode}
                      onValueChange={(value) =>
                        setCodexSandboxMode(
                          value as CodexSandboxMode | CodexDefault,
                        )
                      }
                      className="mt-3 grid gap-2 sm:grid-cols-2"
                    >
                      <LaunchChoice
                        value="default"
                        name="Use Codex settings"
                        title="Use Codex settings"
                        detail="Keep the sandbox configured on your host."
                      />
                      <LaunchChoice
                        value="read-only"
                        name="Read only"
                        title="Read only"
                        detail="Allow reading without writing files (read-only)."
                      />
                      <LaunchChoice
                        value="workspace-write"
                        name="Workspace write"
                        title="Workspace write"
                        detail="Allow writes within the workspace (workspace-write)."
                      />
                      <LaunchChoice
                        value="danger-full-access"
                        name="Full access"
                        title="Full access"
                        detail="Run without the Codex sandbox (danger-full-access)."
                      />
                    </RadioGroup>
                  </div>
                  <div>
                    <h4 className="text-sm font-semibold">Approval policy</h4>
                    <p className="mt-1 text-sm text-muted-foreground">
                      Choose when Codex asks before running commands.
                    </p>
                    <RadioGroup
                      aria-label="Codex approval"
                      disabled={launchLocked}
                      value={codexApprovalPolicy}
                      onValueChange={(value) =>
                        setCodexApprovalPolicy(
                          value as CodexApprovalPolicy | CodexDefault,
                        )
                      }
                      className="mt-3 grid gap-2 sm:grid-cols-2"
                    >
                      <LaunchChoice
                        value="default"
                        name="Use Codex settings"
                        title="Use Codex settings"
                        detail="Keep the approval policy configured on your host."
                      />
                      <LaunchChoice
                        value="untrusted"
                        name="Untrusted"
                        title="Untrusted"
                        detail="Ask for commands outside the trusted set."
                      />
                      <LaunchChoice
                        value="on-failure"
                        name="On failure, deprecated"
                        title="On failure · deprecated"
                        detail="Ask to retry when a sandboxed command fails."
                      />
                      <LaunchChoice
                        value="on-request"
                        name="On request"
                        title="On request"
                        detail="Let Codex decide when to ask."
                      />
                      <LaunchChoice
                        value="never"
                        name="Never ask"
                        title="Never ask"
                        detail="Do not prompt for command approval."
                      />
                    </RadioGroup>
                  </div>
                </>
              )}
              {launchPanel === "opencode" && (
                <>
                  <div>
                    <h4 className="text-sm font-semibold">
                      Permission approval
                    </h4>
                    <p className="mt-1 text-sm text-muted-foreground">
                      Applies to OpenCode, including launches through adk code.
                    </p>
                    <RadioGroup
                      aria-label="OpenCode approval"
                      disabled={launchLocked}
                      value={opencodeAuto ? "auto" : "default"}
                      onValueChange={(value) =>
                        setOpencodeAuto(value === "auto")
                      }
                      className="mt-3 grid gap-2 sm:grid-cols-2"
                    >
                      <LaunchChoice
                        value="default"
                        name="Use OpenCode settings"
                        title="Use OpenCode settings"
                        detail="Keep the permissions configured on your host."
                      />
                      <LaunchChoice
                        value="auto"
                        name="Auto-approve"
                        title="Auto-approve"
                        detail="Approve permissions not explicitly denied. Use only in trusted repos (--auto)."
                      />
                    </RadioGroup>
                  </div>
                  <div>
                    <h4 className="text-sm font-semibold">
                      adk code status strip
                    </h4>
                    <p className="mt-1 text-sm text-muted-foreground">
                      Show or hide the tmux status strip around the Arrakis
                      harness.
                    </p>
                    <RadioGroup
                      aria-label="adk code status strip"
                      disabled={launchLocked}
                      value={adkStatusline ? "on" : "off"}
                      onValueChange={(value) =>
                        setAdkStatusline(value === "on")
                      }
                      className="mt-3 grid gap-2 sm:grid-cols-2"
                    >
                      <LaunchChoice
                        value="off"
                        name="Hidden"
                        title="Hidden"
                        detail="Give the agent the full terminal."
                      />
                      <LaunchChoice
                        value="on"
                        name="Visible"
                        title="Visible"
                        detail="Show the tmux status strip."
                      />
                    </RadioGroup>
                  </div>
                </>
              )}
            </div>
          </div>
        </div>
      </Card>

      {/* Appearance — terminal font size and the inline-code colour. */}
      <Card hidden={activeSection !== "appearance"}>
        <div className="border-b border-border/60 px-5 py-4">
          <h2 className="text-base font-semibold text-foreground">
            Appearance
          </h2>
          <p className="mt-1 text-sm text-muted-foreground">
            How live terminals look, and how copies from them are rendered in
            rich editors.
          </p>
        </div>
        <TerminalFontSizeRow />
        <Row
          label="Inline code colour"
          hint={
            <>
              Text drawn in this colour is pasted as{" "}
              <code className="font-mono text-xs">code</code> into Slack and
              other rich editors. Defaults to Claude Code's inline-code colour.
            </>
          }
        >
          <div className="flex flex-wrap items-center gap-3">
            <input
              type="color"
              aria-label="Inline code colour"
              value={codeColor}
              onChange={(e) => setCodeColor(e.target.value)}
              className="h-9 w-12 cursor-pointer rounded border border-border bg-transparent p-0.5"
            />
            <span className="font-mono text-xs text-muted-foreground">
              {codeColor}
            </span>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setCodeColor(DEFAULT_CODE_HEX)}
              disabled={codeColor.toLowerCase() === DEFAULT_CODE_HEX}
            >
              Reset to default
            </Button>
          </div>
        </Row>
      </Card>

      {/* Repositories — container paths. */}
      <Card hidden={activeSection !== "repositories"}>
        <div className="border-b border-border/60 px-5 py-4">
          <h2 className="text-base font-semibold text-foreground">
            Top-level repositories
          </h2>
          <p className="mt-1 text-sm text-muted-foreground">
            Paths marked here are treated as monorepo containers — their
            sub-repos become the recognized work repos, and the container itself
            is excluded from PR detection.
          </p>
        </div>
        <div className="p-5">
          {isLoading ? (
            <Spinner label="Loading settings…" />
          ) : isError ? (
            <ErrorState
              message={(error as Error)?.message}
              onRetry={() => refetch()}
            />
          ) : (
            <>
              {paths.length === 0 ? (
                <p className="text-xs text-muted-foreground">
                  No container repositories configured yet. Add an absolute path
                  below.
                </p>
              ) : (
                <ul className="space-y-2">
                  {paths.map((p) => (
                    <li
                      key={p}
                      className="flex items-center justify-between gap-2 rounded-md border border-border/70 px-3 py-2"
                    >
                      <span
                        className="truncate font-mono text-xs text-foreground"
                        title={p}
                      >
                        {p}
                      </span>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="shrink-0 text-destructive hover:bg-destructive/10 hover:text-destructive"
                        onClick={() => removePath(p)}
                      >
                        Remove
                      </Button>
                    </li>
                  ))}
                </ul>
              )}

              <form
                className="mt-3 flex flex-col gap-2 sm:flex-row sm:items-center"
                onSubmit={(e) => {
                  e.preventDefault();
                  addPath();
                }}
              >
                <Input
                  aria-label="New container repository path"
                  placeholder="/Users/you/monorepo"
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  className="flex-1 font-mono"
                />
                <Button
                  type="submit"
                  variant="outline"
                  disabled={!draft.trim()}
                >
                  Add
                </Button>
              </form>
            </>
          )}
        </div>
      </Card>
    </div>
  );
}
