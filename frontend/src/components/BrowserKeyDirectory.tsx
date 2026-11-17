import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { useAuth } from "@/lib/useAuth";
import { useBrowserPublication } from "@/lib/browserPublication";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

/** Public discovery candidates, never an authorization decision. */
export function BrowserKeyDirectory({
  showPublication = true,
}: {
  showPublication?: boolean;
}) {
  const { user } = useAuth();
  const { key, publish } = useBrowserPublication();
  const [query, setQuery] = React.useState("");
  const [label, setLabel] = React.useState("My browser");
  const [message, setMessage] = React.useState("");
  const [publicationMessage, setPublicationMessage] = React.useState("");
  const directory = useQuery({
    queryKey: ["user-key-directory", query],
    queryFn: () => api.userKeyDirectory(query),
    enabled: !!user,
  });
  const publishKey = () => {
    setMessage("");
    setPublicationMessage("");
    publish.mutate(label, {
      onSuccess: () =>
        setPublicationMessage(
          "Published. The host owner must still compare your fingerprint and approve this key locally.",
        ),
      onError: (error) => setPublicationMessage(error.message),
    });
  };
  const copy = async (fingerprint: string) => {
    setPublicationMessage("");
    try {
      await navigator.clipboard.writeText(fingerprint);
      setMessage("Fingerprint copied.");
    } catch {
      setMessage("Select and copy the full fingerprint below.");
    }
  };
  return (
    <section className="space-y-3" aria-labelledby="browser-keys-heading">
      <h2 id="browser-keys-heading" className="text-lg font-semibold">
        Users and browser keys
      </h2>
      <p className="text-sm text-muted-foreground">
        Publish this browser's public key so a host owner can find it.
        Publication grants no access. Compare the full fingerprint directly with
        the owner using an independent channel before they choose Trust public
        key on their laptop.
      </p>
      <div className="rounded-md border p-3 space-y-2">
        <h3 className="font-medium">This browser's fingerprint</h3>
        {key.data && (
          <>
            <code className="block break-all select-all text-xs">
              {key.data.kid}
            </code>
            <Button
              size="sm"
              variant="outline"
              onClick={() => void copy(key.data!.kid)}
            >
              Copy my fingerprint
            </Button>
          </>
        )}
        {key.error && <p role="alert">{key.error.message}</p>}
        {showPublication && (
          <>
            <label className="block text-sm">
              Browser name
              <Input
                value={label}
                maxLength={80}
                onChange={(e) => setLabel(e.target.value)}
              />
            </label>
            <Button
              size="sm"
              disabled={
                !user || !key.data || publish.isPending || !label.trim()
              }
              onClick={publishKey}
            >
              {publish.isPending
                ? "Publishing…"
                : "Publish current browser key"}
            </Button>
            {!user && (
              <p className="text-sm">Sign in to publish and browse keys.</p>
            )}
          </>
        )}
      </div>
      {message && (
        <p role="status" className="text-sm">
          {message}
        </p>
      )}
      {showPublication && publicationMessage && (
        <p role="status" className="text-sm">
          {publicationMessage}
        </p>
      )}
      <label className="block text-sm">
        Search users
        <Input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Name or email"
        />
      </label>
      {directory.error && <p role="alert">{directory.error.message}</p>}
      {directory.isLoading && <p>Loading users…</p>}
      <ul className="space-y-3">
        {directory.data?.map((person) => (
          <li key={person.email} className="rounded-md border p-3 space-y-2">
            <p className="font-medium">{person.display_name || person.email}</p>
            <p className="text-sm text-muted-foreground">{person.email}</p>
            {person.keys.length === 0 && (
              <p className="text-sm">No published browser keys.</p>
            )}
            {person.keys.map((published) => (
              <div key={published.kid} className="space-y-1">
                <p className="text-sm">{published.label}</p>
                <code className="block break-all select-all text-xs">
                  {published.kid}
                </code>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void copy(published.kid)}
                >
                  Copy fingerprint
                </Button>
              </div>
            ))}
          </li>
        ))}
      </ul>
    </section>
  );
}
