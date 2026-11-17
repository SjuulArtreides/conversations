import * as React from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { api, ApiError } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

export function ResetPassword() {
  const [searchParams] = useSearchParams();
  const navigate = useNavigate();
  const token = searchParams.get("token") ?? "";
  const [password, setPassword] = React.useState("");
  const [confirmation, setConfirmation] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [submitting, setSubmitting] = React.useState(false);
  const [complete, setComplete] = React.useState(false);

  async function onSubmit(event: React.FormEvent) {
    event.preventDefault();
    setError(null);
    if (password !== confirmation) {
      setError("Passwords do not match.");
      return;
    }

    setSubmitting(true);
    try {
      await api.resetPassword(token, password);
      setComplete(true);
      navigate("/reset-password", { replace: true });
    } catch (err) {
      setError(
        err instanceof ApiError
          ? err.message
          : "Could not reset the password. Please request a new link.",
      );
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div className="mx-auto flex max-w-sm flex-col gap-6 py-10">
      <div className="text-center">
        <h1 className="text-xl font-semibold tracking-tight">
          Reset password
        </h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Reset links expire after 30 minutes and work once.
        </p>
      </div>

      {complete ? (
        <div className="space-y-4 text-center">
          <p className="text-sm text-foreground">
            Your password has been changed.
          </p>
          <Link className="text-sm font-medium text-primary" to="/login">
            Sign in with the new password
          </Link>
        </div>
      ) : !token ? (
        <p className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive-foreground">
          This reset link is invalid. Ask an administrator for a new link.
        </p>
      ) : (
        <form onSubmit={onSubmit} className="flex flex-col gap-3">
          <label className="flex flex-col gap-1 text-sm">
            <span className="font-medium">New password</span>
            <Input
              type="password"
              required
              minLength={8}
              autoComplete="new-password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              placeholder="At least 8 characters"
            />
          </label>
          <label className="flex flex-col gap-1 text-sm">
            <span className="font-medium">Confirm new password</span>
            <Input
              type="password"
              required
              minLength={8}
              autoComplete="new-password"
              value={confirmation}
              onChange={(event) => setConfirmation(event.target.value)}
            />
          </label>

          {error && (
            <p
              role="alert"
              className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive-foreground"
            >
              {error}
            </p>
          )}

          <Button type="submit" disabled={submitting}>
            {submitting ? "Changing password…" : "Change password"}
          </Button>
        </form>
      )}
    </div>
  );
}
