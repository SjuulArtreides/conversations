import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { useBrowserPublication } from "@/lib/browserPublication";
import { connectApprovedHost } from "@/lib/hostDiscovery";

/** How often a published browser asks its account's machines for approval. */
export const APPROVAL_POLL_MS = 5000;

export function PublishBrowserFingerprint() {
  const { key, registration, publish } = useBrowserPublication();
  const published = publish.isSuccess || registration.data === true;
  const connected = useQuery({
    queryKey: ["approved-host", key.data?.kid],
    enabled: published && !!key.data,
    queryFn: connectApprovedHost,
    refetchInterval: (query) => (query.state.data ? false : APPROVAL_POLL_MS),
    retry: false,
  });

  return (
    <div className="mt-3 space-y-2">
      {published && key.data ? (
        <>
          <p role="status">
            {connected.data
              ? "This browser was approved. Loading your sessions…"
              : "Fingerprint published. Waiting for approval on your machine…"}
          </p>
          <code className="block break-all select-all text-xs">
            {key.data.kid}
          </code>
          <p className="text-muted-foreground">
            On your machine, open local Conversations and go to Settings →
            Host access. Under Waiting for approval, choose Compare and trust
            on this fingerprint, compare it, and choose Trust public key. This
            browser connects on its own once the machine approves it.
          </p>
        </>
      ) : (
        <Button
          disabled={!key.data || publish.isPending}
          onClick={() => publish.mutate("My browser")}
        >
          {publish.isPending ? "Publishing…" : "Publish my fingerprint"}
        </Button>
      )}
      {(key.error || publish.error) && (
        <p role="alert" className="text-destructive">
          {key.error?.message ?? publish.error?.message}
        </p>
      )}
    </div>
  );
}
