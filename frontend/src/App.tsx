import {
  Navigate,
  Route,
  Routes,
  useLocation,
  useNavigate,
  useParams,
} from "react-router-dom";
import * as React from "react";
import {
  saveHostLinkForLogin,
  takeHostLinkAfterLogin,
} from "./lib/hostLinkReturn";
import { AppShell } from "./components/AppShell";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { SessionList } from "./routes/SessionList";
import { SessionRoute } from "./routes/SessionRoute";
import { ProjectList } from "./routes/ProjectList";
import { ProjectDetail } from "./routes/ProjectDetail";
import { Settings } from "./routes/Settings";
import { Login } from "./routes/Login";
import { HostLink } from "./routes/HostLink";
import {
  PrivateSessionList,
  PrivateSessionView,
} from "./routes/PrivateSessions";
import { ResetPassword } from "./routes/ResetPassword";
import { EmptyState, ErrorState, Spinner } from "./components/states";
import { useAuth } from "./lib/useAuth";
import { refreshLocalHost, useLocalHost } from "./lib/localHost";
import { useKeyboardInsetVar } from "./lib/useVisualViewport";

function HostLinkSignIn() {
  const navigate = useNavigate();
  const { hash } = useLocation();
  React.useEffect(() => {
    saveHostLinkForLogin(hash);
    navigate("/login", { replace: true });
  }, [hash, navigate]);
  return <Spinner label="Opening sign in…" />;
}

export default function App() {
  const { isLoading, authEnabled, user } = useAuth();
  const host = useLocalHost();
  const location = useLocation();
  const navigate = useNavigate();

  // Track the on-screen keyboard so fixed panels (e.g. the docked chat rail)
  // can sit above it on mobile instead of behind it. No-op on desktop.
  useKeyboardInsetVar();

  // When auth is on and nobody is signed in, gate the data routes behind /login.
  // When auth is off (authEnabled === false), everything is open as before.
  const needsLogin = authEnabled && !user;
  React.useEffect(() => {
    if (!isLoading && !needsLogin && !host.local && location.pathname === "/") {
      const hash = takeHostLinkAfterLogin();
      if (hash) navigate({ pathname: "/host-link", hash }, { replace: true });
    }
  }, [isLoading, needsLogin, host.local, location.pathname, navigate]);

  const protect = (el: React.ReactNode) =>
    needsLogin ? <Navigate to="/login" replace /> : el;

  return (
    <AppShell>
      {host.local && !host.authenticated ? (
        <div className="space-y-4">
          <ErrorState
            title="Connect this browser once"
            message="Run arrakis-conversations host web on this laptop to open an authorized browser tab. This browser will then remember access across restarts."
            onRetry={() => void refreshLocalHost()}
          />
          {host.error && (
            <p role="alert" className="text-center text-sm text-destructive">
              {host.error}
            </p>
          )}
        </div>
      ) : isLoading ? (
        <Spinner label="Loading…" />
      ) : (
        // Per-route net: a crash in one route renders a fallback inside the
        // shell (nav stays usable) and self-heals when the pathname changes.
        <ErrorBoundary
          resetKeys={[location.pathname]}
          title="This page crashed"
        >
          <Routes>
            <Route
              path="/login"
              element={host.local ? <Navigate to="/" replace /> : <Login />}
            />
            <Route
              path="/reset-password"
              element={
                host.local ? <Navigate to="/" replace /> : <ResetPassword />
              }
            />
            <Route path="/" element={protect(<SessionList />)} />
            <Route
              path="/sessions/:id"
              element={protect(
                host.local ? <PrivateSessionView /> : <SessionRoute />,
              )}
            />
            {/* Legacy: live and stored share one URL now. Redirect old /live/:id
                links (bookmarks, copied URLs) to the unified session route. */}
            <Route path="/live/:id" element={<LiveRedirect />} />
            <Route path="/projects" element={protect(<ProjectList />)} />
            <Route path="/projects/:id" element={protect(<ProjectDetail />)} />
            <Route path="/settings" element={protect(<Settings />)} />
            <Route
              path="/host-link"
              element={
                host.local ? (
                  <Navigate to="/settings" replace />
                ) : needsLogin ? (
                  <HostLinkSignIn />
                ) : (
                  <HostLink />
                )
              }
            />
            <Route path="/private" element={protect(<PrivateSessionList />)} />
            <Route
              path="/private/:id"
              element={protect(<PrivateSessionView />)}
            />
            <Route
              path="*"
              element={
                <EmptyState
                  title="Page not found"
                  message="That route doesn’t exist."
                />
              }
            />
          </Routes>
        </ErrorBoundary>
      )}
    </AppShell>
  );
}

/** Redirect a legacy `/live/:id` URL to the unified `/sessions/:id` route. */
function LiveRedirect() {
  const { id = "" } = useParams();
  return <Navigate to={`/sessions/${encodeURIComponent(id)}`} replace />;
}
