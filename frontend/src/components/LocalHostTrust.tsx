import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type KeyDirectoryUser, type PublishedBrowserKey } from "@/lib/api";
import { hostControl } from "@/lib/hostControl";
import { localRequest, useLocalHost } from "@/lib/localHost";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import { Collapsible } from "./Collapsible";
import { ErrorState, Spinner } from "./states";

interface LocalStatus {
  descriptor: string;
  server: string;
  credentials: Array<{
    kid: string;
    label: string;
    revoked: boolean;
    grants: Array<{ cap: string; session: string }>;
  }>;
}

// PR refresh (pr.sync / pr.unlink) is no longer its own permission:
// `session.write` — the capability that lets a key drive a session's
// terminal — also lets it refresh that session's linked pull requests.
// There is no separate "Refresh pull-request status" checkbox.
const browserPermissions = [
  ["history.read", "Read session history"],
  [
    "session.write",
    "Control running terminals (and refresh their pull requests)",
  ],
  ["session.start", "Start sessions"],
  ["session.reopen", "Reopen sessions"],
] as const;

const permissions = [
  ...browserPermissions,
  ["session.pr_comment", "Post pull-request comments"],
  ["session.manage_sharing", "Share sessions with teammates"],
  ["host.settings_read", "Read host launch settings"],
  ["host.settings_write", "Change host launch settings"],
  ["passkey.manage", "Invite and approve session keys"],
  ["browser_trust.manage", "Approve browser keys remotely with a passkey"],
] as const;

const PERMISSION_LABELS: Record<string, string> = Object.fromEntries(
  permissions.map(([cap, text]) => [cap, text]),
);

/**
 * Capabilities the browser's delegation requests (see `DELEGATION_CAPS` in
 * `lib/hostControl.ts`) that are not in the historical four-cap set. An owner
 * credential missing any of these cannot drive the matching UI feature from
 * the hosted dashboard: PR comment, share links, and host launch settings all
 * fail with "operation outside the delegation's scope". The local management
 * page offers an explicit, cookie-authorized upgrade to add them without
 * re-enrolling the passkey — the only path a running host has for widening
 * an already-approved key's authority.
 *
 * PR refresh is intentionally absent here: it now follows `session.write`,
 * which the historical four-cap set already includes, so a legacy owner key
 * already refreshes pull requests without any upgrade.
 */
const OWNER_ONLY_CAPS = [
  "session.pr_comment",
  "session.manage_sharing",
  "host.settings_read",
  "host.settings_write",
  "passkey.manage",
  "browser_trust.manage",
] as const;

function describeGrant(cap: string, session: string): string {
  const label = PERMISSION_LABELS[cap] ?? cap;
  if (!session || session === "*") return label;
  return `${label} (scoped to ${session})`;
}

/** Directory entries are candidates; only this owner's explicit comparison grants access. */
export function LocalHostTrust({ remoteHost }: { remoteHost?: { id: string; label: string } } = {}) {
  const host = useLocalHost();
  const cache = useQueryClient();
  const [addOpen, setAddOpen] = React.useState(false);
  const [query, setQuery] = React.useState("");
  const [selected, setSelected] = React.useState<{
    email: string;
    key: PublishedBrowserKey;
  } | null>(null);
  const [label, setLabel] = React.useState("");
  const [compared, setCompared] = React.useState(false);
  const [grants, setGrants] = React.useState<string[]>(["history.read"]);
  const local = host.local && host.authenticated && !remoteHost;
  const searchInput = React.useRef<HTMLInputElement | null>(null);

  const status = useQuery({
    queryKey: ["local-host-status"],
    queryFn: () => localRequest<LocalStatus>("/api/status"),
    enabled: local,
  });
  const directory = useQuery({
    queryKey: ["host-trust-directory", remoteHost?.id, query],
    queryFn: () =>
      remoteHost
        ? api.userKeyDirectory(query).then((users) => ({ users }))
        : localRequest<{ users: KeyDirectoryUser[] }>(
            `/api/users?q=${encodeURIComponent(query)}`,
          ),
    enabled: (local || !!remoteHost) && addOpen,
    retry: false,
  });
  const trust = useMutation({
    mutationFn: () => {
      if (!selected || !compared || !grants.length || !label.trim())
        throw new Error(
          "Select and independently compare a key before trusting it.",
        );
      const request = {
        kid: selected.key.kid,
        public_key: selected.key.public_key,
        label: label.trim(),
        compared: true,
        grants,
      };
      if (remoteHost) return hostControl.trustPublishedBrowser(remoteHost.id, request);
      return localRequest<{ kid: string }>("/api/trust", {
        method: "POST",
        body: JSON.stringify(request),
      });
    },
    onSuccess: () => {
      setCompared(false);
      void cache.invalidateQueries({ queryKey: ["local-host-status"] });
    },
  });
  const upgrade = useMutation({
    mutationFn: (variables: { kid: string; grants: readonly string[] }) =>
      localRequest<{ kid: string; added: string[] }>("/api/trust/upgrade", {
        method: "POST",
        body: JSON.stringify(variables),
      }),
    onSuccess: () =>
      void cache.invalidateQueries({ queryKey: ["local-host-status"] }),
  });

  function selectCandidate(person: KeyDirectoryUser, key: PublishedBrowserKey) {
    setSelected({ email: person.email, key: { ...key } });
    setLabel(`${person.email}: ${key.label}`.slice(0, 64));
    setCompared(false);
    // Default to the minimum useful grant: read-only access to session
    // history. The user must explicitly tick "Control running terminals"
    // (session.write) to grant terminal control AND PR refresh, since
    // refresh now rides on write. Removing the obsolete standalone
    // pr_sync cap MUST NOT silently promote formerly-refresh-only trust
    // to full terminal control.
    setGrants(["history.read"]);
    trust.reset();
  }

  function openAddPerson() {
    setAddOpen(true);
    // Focus the search field after it mounts so keyboard users can type
    // immediately. Safe in tests under JSDOM.
    requestAnimationFrame(() => searchInput.current?.focus());
  }

  function closeAddPerson() {
    setAddOpen(false);
    setSelected(null);
    setCompared(false);
    setQuery("");
    trust.reset();
  }

  if (!local && !remoteHost) return null;

  const candidates = (directory.data?.users ?? []).filter(
    (person) => person.keys.length > 0,
  );

  return (
    <section className="space-y-5" aria-labelledby="local-trust-heading">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h2 id="local-trust-heading" className="text-lg font-semibold">
            {remoteHost ? `Browser access to ${remoteHost.label}` : "Access to this host"}
          </h2>
          <p className="mt-1 text-sm text-muted-foreground">
            {remoteHost
              ? "Compare a published browser key's fingerprint and verify with a passkey. Enable remote browser approvals in the machine's local Host access settings first."
              : "These are the browser keys you have approved on this laptop. Only keys listed here can sign in to your host."}
          </p>
        </div>
        {!addOpen && (
          <Button
            variant="secondary"
            size="sm"
            onClick={openAddPerson}
            aria-expanded={false}
            aria-controls="local-trust-add-person"
          >
            Add person
          </Button>
        )}
      </div>

      {status.isLoading && <Spinner label="Loading approved keys…" />}
      {status.error && (
        <ErrorState
          message={status.error.message}
          onRetry={() => void status.refetch()}
        />
      )}

      {status.data && (
        <div className="space-y-3">
          <h3 className="text-sm font-semibold">People with access</h3>
          {status.data.credentials.length === 0 ? (
            <p className="rounded-lg border border-dashed border-border p-4 text-sm text-muted-foreground">
              No one has access yet. Choose <strong>Add person</strong> above to
              approve a browser key.
            </p>
          ) : (
            <ul className="space-y-2">
              {status.data.credentials.map((credential) => {
                const held = new Set(
                  credential.grants.map((grant) => grant.cap),
                );
                const missing = OWNER_ONLY_CAPS.filter((cap) => !held.has(cap));
                const displayed =
                  credential.grants
                    .map((grant) => describeGrant(grant.cap, grant.session))
                    .join(" · ") || "No active permissions";
                return (
                  <li
                    key={credential.kid}
                    className="rounded-lg border border-border p-3 space-y-2"
                  >
                    <div className="flex items-center justify-between gap-2">
                      <p className="text-sm font-medium">{credential.label}</p>
                      {credential.revoked && (
                        <span className="rounded bg-muted px-2 py-0.5 text-xs text-muted-foreground">
                          Revoked
                        </span>
                      )}
                    </div>
                    <p className="text-xs text-muted-foreground">{displayed}</p>

                    {!credential.revoked && missing.length > 0 && (
                      <Collapsible label="Add permissions">
                        <div className="space-y-2">
                          <p className="text-xs text-muted-foreground">
                            Allow only the permissions you need for this key.
                            Existing permissions stay unchanged.
                          </p>
                          <div className="flex flex-wrap gap-2">
                            {missing.map((cap) => (
                              <Button
                                key={cap}
                                size="sm"
                                variant="secondary"
                                disabled={
                                  upgrade.isPending ||
                                  (cap === "host.settings_write" &&
                                    !held.has("host.settings_read"))
                                }
                                onClick={() =>
                                  upgrade.mutate({
                                    kid: credential.kid,
                                    grants: [cap],
                                  })
                                }
                              >
                                {upgrade.isPending &&
                                upgrade.variables?.kid === credential.kid &&
                                upgrade.variables.grants.includes(cap)
                                  ? "Allowing…"
                                  : `Allow: ${PERMISSION_LABELS[cap] ?? cap}`}
                              </Button>
                            ))}
                          </div>
                          {missing.includes("host.settings_write") &&
                            !held.has("host.settings_read") && (
                              <p className="text-xs text-muted-foreground">
                                To change host launch settings, first allow
                                reading them.
                              </p>
                            )}
                        </div>
                      </Collapsible>
                    )}
                    {upgrade.error &&
                      upgrade.variables?.kid === credential.kid && (
                        <p role="alert" className="text-xs text-destructive">
                          {upgrade.error.message}
                        </p>
                      )}
                    {upgrade.isSuccess &&
                      upgrade.variables?.kid === credential.kid && (
                        <p role="status" className="text-xs">
                          {upgrade.data.added.length
                            ? "Permission added"
                            : "Already allowed"}
                          :{" "}
                          {PERMISSION_LABELS[upgrade.variables.grants[0]] ??
                            upgrade.variables.grants[0]}
                          . Renew access to this host in the hosted app with
                          your passkey (Touch ID) to use it.
                        </p>
                      )}

                    <Collapsible label="Key details">
                      <div className="space-y-1">
                        <p className="text-xs text-muted-foreground">
                          Full key fingerprint:
                        </p>
                        <code className="block break-all select-all text-xs">
                          {credential.kid}
                        </code>
                      </div>
                    </Collapsible>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      )}

      {status.data && (
        <Collapsible label="Connection details">
          <div className="space-y-1">
            <p className="text-xs text-muted-foreground">
              Host descriptor — share directly with someone you are about to
              approve, over an independent channel.
            </p>
            <code className="block break-all select-all text-xs">
              {status.data.descriptor}
            </code>
          </div>
        </Collapsible>
      )}

      {addOpen && (
        <div
          id="local-trust-add-person"
          className="space-y-4 rounded-lg border border-border p-4"
        >
          <div className="flex items-start justify-between gap-3">
            <div>
              <h3 className="text-sm font-semibold">Add a person</h3>
              <p className="mt-1 text-sm text-muted-foreground">
                Search for someone who has published a browser key. Publishing a
                key does not grant access — you must compare the full
                fingerprint with the person over an independent channel and then
                approve it below. Candidates with no published key are hidden.
              </p>
            </div>
            <Button variant="ghost" size="sm" onClick={closeAddPerson} disabled={trust.isPending}>
              Cancel
            </Button>
          </div>

          <label className="block space-y-1 text-sm">
            Search published keys
            <Input
              ref={searchInput}
              value={query}
              disabled={trust.isPending}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Name or email"
              maxLength={200}
            />
          </label>
          {directory.isLoading && <Spinner label="Loading published keys…" />}
          {directory.error && (
            <div role="alert">
              <ErrorState
                message={directory.error.message}
                onRetry={() => void directory.refetch()}
              />
            </div>
          )}
          {directory.data && candidates.length === 0 && (
            <p className="rounded-md border border-dashed border-border p-3 text-sm text-muted-foreground">
              No published browser keys match
              {query.trim() ? ` "${query.trim()}"` : " that search"}. Ask the
              person to publish their current browser's key from their account
              Settings, then try again.
            </p>
          )}

          <div className="space-y-3">
            {candidates.map((person) => (
              <div
                key={person.email}
                className="rounded-md border border-border p-3 space-y-2"
              >
                <p className="text-sm font-medium">
                  {person.display_name || person.email}
                </p>
                <p className="text-xs text-muted-foreground">
                  {person.email} — directory name is not proof of identity.
                </p>
                {person.keys.map((key) => (
                  <label
                    key={key.kid}
                    className="flex items-start gap-3 text-sm"
                  >
                    <input
                      type="radio"
                      name="published-key"
                      disabled={trust.isPending}
                      checked={
                        selected?.email === person.email &&
                        selected.key.kid === key.kid &&
                        selected.key.public_key === key.public_key
                      }
                      onChange={() => selectCandidate(person, key)}
                      aria-label={`Select ${key.label} for ${person.email}`}
                    />
                    <span className="min-w-0">
                      <span className="block">{key.label}</span>
                    </span>
                  </label>
                ))}
              </div>
            ))}
          </div>

          {selected && (
            <div className="space-y-4 rounded-md border border-border bg-muted/30 p-4">
              <h4 className="font-semibold">
                Compare this fingerprint independently
              </h4>
              <p className="text-sm text-muted-foreground">
                The directory can misidentify a person. Compare the full
                fingerprint below with the one the person sees in their browser
                Settings over a call or another independent channel.
              </p>
              <code className="block break-all select-all text-xs">
                {selected.key.kid}
              </code>
              <label className="block space-y-1 text-sm">
                Name for this approved key
                <Input
                  value={label}
                  onChange={(event) => setLabel(event.target.value)}
                  maxLength={64}
                  disabled={trust.isPending}
                />
              </label>
              <fieldset disabled={trust.isPending} className="space-y-2">
                <legend className="mb-2 text-sm font-medium">
                  Permissions on this host
                </legend>
                {(remoteHost ? browserPermissions : permissions).map(([cap, text]) => (
                  <label key={cap} className="flex items-center gap-2 text-sm">
                    <input
                      type="checkbox"
                      checked={grants.includes(cap)}
                      onChange={(event) =>
                        setGrants((previous) =>
                          event.target.checked
                            ? [...previous, cap]
                            : previous.filter((value) => value !== cap),
                        )
                      }
                    />
                    {text}
                  </label>
                ))}
              </fieldset>
              <label className="flex items-start gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={compared}
                  disabled={trust.isPending}
                  onChange={(event) => setCompared(event.target.checked)}
                />
                I independently compared this exact fingerprint with its owner.
              </label>
              <Button
                disabled={
                  !compared ||
                  !grants.length ||
                  !label.trim() ||
                  trust.isPending
                }
                onClick={() => trust.mutate()}
              >
                {trust.isPending ? "Trusting…" : "Trust public key"}
              </Button>
              {trust.error && (
                <p role="alert" className="text-sm text-destructive">
                  {trust.error.message}
                </p>
              )}
              {trust.isSuccess && (
                <p role="status" className="text-sm">
                  Public key trusted with the selected permissions.
                </p>
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
