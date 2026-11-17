import * as React from "react";
import { ErrorState } from "./states";

interface Props {
  children: React.ReactNode;
  /**
   * Optional custom fallback. Receives the caught error and a `reset` callback
   * that clears the boundary so a re-render can re-mount the subtree. When
   * omitted, a generic {@link ErrorState} with a "Try again" button is shown.
   */
  fallback?: (error: Error, reset: () => void) => React.ReactNode;
  /**
   * When any value in this array changes between renders, the boundary clears a
   * captured error automatically. The per-route boundary passes the current
   * pathname so navigating away from a crashed route recovers without a full
   * page reload.
   */
  resetKeys?: ReadonlyArray<unknown>;
  /** Label shown above the message in the default fallback. */
  title?: string;
}

interface State {
  error: Error | null;
}

/**
 * Catches render/lifecycle errors in its subtree and shows a fallback instead
 * of letting React unmount the whole app to a blank page. There is no global
 * `componentDidCatch` in function components, so this stays a class — the one
 * place a class component is unavoidable.
 *
 * Two usages exist: a top-level boundary around the entire app (catches a shell
 * crash) and a per-route boundary keyed on the pathname (catches a single
 * route's crash and self-heals on navigation, leaving the chrome intact).
 */
export class ErrorBoundary extends React.Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidUpdate(prev: Props) {
    // Clear a captured error when the reset keys change (e.g. route nav), so the
    // subtree re-mounts fresh instead of staying stuck on the fallback.
    if (this.state.error && !shallowEqual(prev.resetKeys, this.props.resetKeys)) {
      this.setState({ error: null });
    }
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    // No telemetry pipeline here; surface it to the console for local debugging.
    console.error("ErrorBoundary caught an error", error, info.componentStack);
  }

  reset = () => this.setState({ error: null });

  render() {
    const { error } = this.state;
    if (error) {
      if (this.props.fallback) {
        return this.props.fallback(error, this.reset);
      }
      return (
        <ErrorState
          title={this.props.title ?? "Something went wrong"}
          message={error.message || "An unexpected error occurred."}
          onRetry={this.reset}
        />
      );
    }
    return this.props.children;
  }
}

function shallowEqual(
  a: ReadonlyArray<unknown> | undefined,
  b: ReadonlyArray<unknown> | undefined,
): boolean {
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  return a.every((v, i) => Object.is(v, b[i]));
}
