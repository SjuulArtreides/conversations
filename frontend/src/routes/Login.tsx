import * as React from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "@/lib/api";
import { AUTH_QUERY_KEY, useAuth } from "@/lib/useAuth";
import { enableTerminalCache } from "@/lib/terminalCache";
import { Button, buttonVariants } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/states";

type Mode = "login" | "register";

export function Login() {
  const { isLoading, authEnabled, user, googleEnabled, passwordEnabled } =
    useAuth();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const [mode, setMode] = React.useState<Mode>("login");
  const [email, setEmail] = React.useState("");
  const [password, setPassword] = React.useState("");
  const [displayName, setDisplayName] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [submitting, setSubmitting] = React.useState(false);

  // With SSO present, the password path must not compete with the primary
  // Google action.
  const activeTab = googleEnabled
    ? "bg-accent text-accent-foreground"
    : "bg-primary text-primary-foreground";

  if (isLoading) return <Spinner label="Loading…" />;
  // Auth disabled, or already signed in — nothing to do here.
  if (!authEnabled || user) return <Navigate to="/" replace />;

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    setError(null);
    setSubmitting(true);
    try {
      if (mode === "register") {
        await api.register({
          email,
          password,
          display_name: displayName.trim() || undefined,
        });
      } else {
        await api.login({ email, password });
      }
      enableTerminalCache();
      await queryClient.invalidateQueries({ queryKey: AUTH_QUERY_KEY });
      navigate("/", { replace: true });
    } catch (err) {
      setError(
        err instanceof ApiError
          ? err.message
          : "Something went wrong. Please try again.",
      );
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div className="mx-auto flex max-w-sm flex-col gap-6 py-10">
      <div className="text-center">
        <div className="mx-auto mb-3 grid h-12 w-12 place-items-center rounded-xl bg-primary/15 text-2xl text-primary">
          💬
        </div>
        <h1 className="text-xl font-semibold tracking-tight">
          Conversations
        </h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Sign in to view and share Claude Code sessions.
        </p>
      </div>

      {googleEnabled && (
        <a href="/auth/google/login" className={buttonVariants({ size: "lg" })}>
          Continue with Google
        </a>
      )}

      {passwordEnabled && googleEnabled && (
        <div className="flex items-center gap-3 text-xs text-muted-foreground">
          <span className="h-px flex-1 bg-border" />
          or use email
          <span className="h-px flex-1 bg-border" />
        </div>
      )}

      {passwordEnabled && (
        <>
          <div className="flex rounded-lg border border-border p-1 text-sm">
            <button
              type="button"
              onClick={() => {
                setMode("login");
                setError(null);
              }}
              className={`flex-1 rounded-md px-3 py-1.5 font-medium transition-colors ${
                mode === "login"
                  ? activeTab
                  : "text-muted-foreground hover:text-foreground"
              }`}
            >
              Sign in
            </button>
            <button
              type="button"
              onClick={() => {
                setMode("register");
                setError(null);
              }}
              className={`flex-1 rounded-md px-3 py-1.5 font-medium transition-colors ${
                mode === "register"
                  ? activeTab
                  : "text-muted-foreground hover:text-foreground"
              }`}
            >
              Create account
            </button>
          </div>

          <form onSubmit={onSubmit} className="flex flex-col gap-3">
            {mode === "register" && (
              <label className="flex flex-col gap-1 text-sm">
                <span className="font-medium">Name (optional)</span>
                <Input
                  type="text"
                  autoComplete="name"
                  value={displayName}
                  onChange={(e) => setDisplayName(e.target.value)}
                  placeholder="Jane Doe"
                />
              </label>
            )}
            <label className="flex flex-col gap-1 text-sm">
              <span className="font-medium">Email</span>
              <Input
                type="email"
                required
                autoComplete="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder="you@example.com"
              />
            </label>
            <label className="flex flex-col gap-1 text-sm">
              <span className="font-medium">Password</span>
              <Input
                type="password"
                required
                minLength={mode === "register" ? 8 : undefined}
                autoComplete={
                  mode === "register" ? "new-password" : "current-password"
                }
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder={
                  mode === "register" ? "At least 8 characters" : "••••••••"
                }
              />
            </label>

            {error && (
              <p className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive-foreground">
                {error}
              </p>
            )}

            <Button
              type="submit"
              variant={googleEnabled ? "secondary" : "default"}
              disabled={submitting}
              className="mt-1"
            >
              {submitting
                ? "Please wait…"
                : mode === "register"
                  ? "Create account"
                  : "Sign in"}
            </Button>
          </form>
        </>
      )}
    </div>
  );
}
