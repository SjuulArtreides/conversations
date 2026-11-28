import { Button } from "@/components/ui/button";
import { useBrowserPublication } from "@/lib/browserPublication";

export function PublishBrowserFingerprint() {
  const { key, registration, publish } = useBrowserPublication();
  const published = publish.isSuccess || registration.data === true;

  return (
    <div className="mt-3 space-y-2">
      {published && key.data ? (
        <>
          <p role="status">Fingerprint published.</p>
          <code className="block break-all select-all text-xs">{key.data.kid}</code>
          <p className="text-muted-foreground">
            On your machine, open local Conversations and go to Settings →
            Access to this host → Add person. Find your account, compare this
            fingerprint, and choose Trust public key.
          </p>
          <p className="text-muted-foreground">
            To connect this browser, copy the setup link from Host access in
            your approved browser and open it here.
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
