import * as React from "react";
import { useNavigate } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { parseHostLink } from "@/routes/HostLink";

export function ConnectDevice() {
  const navigate = useNavigate();
  const [open, setOpen] = React.useState(false);
  const [link, setLink] = React.useState("");
  const [error, setError] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const connect = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const url = new URL(link.trim());
      if (
        url.origin !== window.location.origin ||
        url.pathname !== "/host-link" ||
        !(await parseHostLink(url.hash))
      ) {
        throw new Error("Invalid setup link");
      }
      navigate({ pathname: "/host-link", hash: url.hash });
    } catch {
      setError(
        "Paste a setup link from your laptop for this Conversations server.",
      );
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="mt-3 space-y-3">
      <Button onClick={() => setOpen(!open)} aria-expanded={open}>
        Connect this device
      </Button>
      {open && (
        <form onSubmit={(event) => void connect(event)} className="space-y-2">
          <p>
            On your laptop, open Settings, choose Add a device, and copy the
            setup link. Paste it here to continue.
          </p>
          <label className="block">
            Setup link
            <Input
              value={link}
              onChange={(event) => setLink(event.target.value)}
              type="url"
              autoComplete="off"
              autoCapitalize="none"
              spellCheck={false}
              required
            />
          </label>
          <Button type="submit" disabled={busy || !link.trim()}>
            {busy ? "Checking link…" : "Continue to passkey setup"}
          </Button>
          {error && <p role="alert">{error}</p>}
        </form>
      )}
    </div>
  );
}
