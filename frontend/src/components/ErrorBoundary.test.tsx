import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import * as React from "react";
import { ErrorBoundary } from "./ErrorBoundary";

/** A child that throws on render while `boom` is true. */
function Boom({ boom, label = "ok" }: { boom: boolean; label?: string }) {
  if (boom) throw new Error("kaboom");
  return <div>{label}</div>;
}

describe("ErrorBoundary", () => {
  // The boundary logs caught errors; silence it so the test output stays clean.
  let consoleError: ReturnType<typeof vi.spyOn>;
  beforeEach(() => {
    consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
  });
  afterEach(() => {
    consoleError.mockRestore();
  });

  it("renders children when nothing throws", () => {
    render(
      <ErrorBoundary>
        <Boom boom={false} />
      </ErrorBoundary>,
    );
    expect(screen.getByText("ok")).toBeInTheDocument();
  });

  it("shows the default fallback with the error message when a child throws", () => {
    render(
      <ErrorBoundary title="This page crashed">
        <Boom boom />
      </ErrorBoundary>,
    );
    expect(screen.getByText("This page crashed")).toBeInTheDocument();
    expect(screen.getByText("kaboom")).toBeInTheDocument();
  });

  it("clears the error via the Try again button so the subtree can recover", async () => {
    const user = userEvent.setup();

    function Harness() {
      const [boom, setBoom] = React.useState(true);
      return (
        <ErrorBoundary
          fallback={(error, reset) => (
            <button
              onClick={() => {
                setBoom(false);
                reset();
              }}
            >
              recover: {error.message}
            </button>
          )}
        >
          <Boom boom={boom} label="recovered" />
        </ErrorBoundary>
      );
    }

    render(<Harness />);
    await user.click(screen.getByRole("button", { name: /recover: kaboom/i }));
    expect(screen.getByText("recovered")).toBeInTheDocument();
  });

  it("auto-resets when resetKeys change (route navigation)", () => {
    const { rerender } = render(
      <ErrorBoundary resetKeys={["/a"]}>
        <Boom boom />
      </ErrorBoundary>,
    );
    expect(screen.getByText("kaboom")).toBeInTheDocument();

    // Navigating to a new path changes the reset key; the no-longer-throwing
    // child should render.
    rerender(
      <ErrorBoundary resetKeys={["/b"]}>
        <Boom boom={false} label="next page" />
      </ErrorBoundary>,
    );
    expect(screen.getByText("next page")).toBeInTheDocument();
  });
});
