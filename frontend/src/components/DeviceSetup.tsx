import * as React from "react";
import { QRCodeSVG } from "qrcode.react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { localRequest } from "@/lib/localHost";
import { Button } from "@/components/ui/button";

type Pending = { cred_id: string; label: string; grants: string[] };
export function DeviceSetup() {
  const cache = useQueryClient();
  const [open, setOpen] = React.useState(false);
  const [link, setLink] = React.useState("");
  const [message, setMessage] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const pending = useQuery({
    queryKey: ["device-pending"],
    queryFn: () => localRequest<{ devices: Pending[] }>("/api/devices/pending"),
    refetchInterval: 3000,
    retry: false,
  });
  const start = async () => {
    setBusy(true);
    setMessage("");
    try {
      const result = await localRequest<{ link: string }>(
        "/api/devices/start",
        { method: "POST" },
      );
      setLink(result.link);
    } catch (error) {
      setMessage((error as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(link);
      setMessage("Setup link copied.");
    } catch {
      setMessage("Could not copy the link. Use the QR code or try again.");
    }
  };
  const configure = async () => {
    setBusy(true);
    setMessage("");
    try {
      await localRequest("/api/devices/configure", { method: "POST" });
      await start();
    } catch (error) {
      setMessage((error as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const decide = async (cred_id: string, approve: boolean) => {
    setBusy(true);
    setMessage("");
    try {
      await localRequest(`/api/devices/${approve ? "approve" : "reject"}`, {
        method: "POST",
        body: JSON.stringify({ cred_id }),
      });
      setMessage(
        approve
          ? "Device approved. Continue on your phone."
          : "Device rejected.",
      );
      await cache.invalidateQueries({ queryKey: ["device-pending"] });
    } catch (error) {
      setMessage((error as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const devices = pending.data?.devices ?? [];
  return (
    <section id="devices" className="space-y-3 border-b border-border/60 pb-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="font-semibold">Connect a phone or another device</h2>
          <p className="mt-1 text-sm text-muted-foreground">
            Use a passkey and approve the device on this laptop.
          </p>
        </div>
        <Button variant="outline" onClick={() => setOpen(!open)}>
          {open ? "Close device setup" : "Add a device"}
        </Button>
      </div>
      {(open || devices.length > 0 || message) && (
        <div className="space-y-4 rounded-lg border border-border p-4 text-sm">
          {open && !link && (
            <>
              <p>Start a short-lived setup link, then open it on your phone.</p>
              <Button onClick={() => void start()} disabled={busy}>
                Create setup code
              </Button>
            </>
          )}
          {open && link && (
            <>
              <p>Scan this code with your phone. Keep this link private.</p>
              <div className="inline-block max-w-full rounded-lg bg-white p-3">
                <QRCodeSVG
                  value={link}
                  size={200}
                  className="h-auto max-w-full"
                  title="Phone setup QR code"
                />
              </div>
              <div>
                <Button variant="outline" onClick={() => void copy()}>
                  Copy setup link
                </Button>
                <Button
                  className="ml-2"
                  variant="outline"
                  disabled={busy}
                  onClick={() => void start()}
                >
                  Create a new code
                </Button>
              </div>
            </>
          )}
          {devices.map((device) => (
            <div
              key={device.cred_id}
              className="space-y-2 rounded-lg border border-primary/40 p-3"
            >
              <h3 className="font-semibold">Device waiting for approval</h3>
              <p>Match this code with the one shown on your phone:</p>
              <code className="block break-all text-lg font-semibold tracking-wider">
                {device.cred_id.slice(-12)}
              </code>
              <details>
                <summary>Full device identifier</summary>
                <code className="break-all text-xs">{device.cred_id}</code>
              </details>
              <p>
                Approval lets this device read private sessions, type in
                terminals, and start or reopen sessions.
              </p>
              <div className="flex flex-wrap gap-2">
                <Button
                  disabled={busy}
                  onClick={() => void decide(device.cred_id, true)}
                >
                  Approve device
                </Button>
                <Button
                  variant="outline"
                  disabled={busy}
                  onClick={() => void decide(device.cred_id, false)}
                >
                  Reject
                </Button>
              </div>
            </div>
          ))}
          {open && link && devices.length === 0 && (
            <p className="text-muted-foreground">
              Waiting for your phone to create its passkey…
            </p>
          )}
          {pending.isError && (
            <p role="alert">
              Could not check for pending devices. Check that your host is
              running.
            </p>
          )}
          {message && (
            <div role="status">
              <p className="break-words">{message}</p>
              {/passkeys are not configured|webauthn\.rp_id/i.test(message) && (
                <Button
                  className="mt-2"
                  variant="outline"
                  onClick={() => void configure()}
                  disabled={busy}
                >
                  Set up passkeys for this server
                </Button>
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
