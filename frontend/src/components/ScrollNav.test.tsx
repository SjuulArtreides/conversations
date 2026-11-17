import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ScrollNav } from "./ScrollNav";

describe("ScrollNav — visibility", () => {
  it("renders nothing when both showUp and showDown are false", () => {
    const { container } = render(
      <ScrollNav showUp={false} showDown={false} onUp={vi.fn()} onDown={vi.fn()} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("shows only the ↑ button when showUp=true and showDown=false", () => {
    render(
      <ScrollNav showUp showDown={false} onUp={vi.fn()} onDown={vi.fn()} />,
    );
    expect(screen.getByRole("button", { name: /scroll to top/i })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /scroll to bottom/i })).not.toBeInTheDocument();
  });

  it("shows only the ↓ button when showDown=true and showUp=false", () => {
    render(
      <ScrollNav showUp={false} showDown onUp={vi.fn()} onDown={vi.fn()} />,
    );
    expect(screen.getByRole("button", { name: /scroll to bottom/i })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /scroll to top/i })).not.toBeInTheDocument();
  });

  it("shows both buttons when showUp=true and showDown=true", () => {
    render(
      <ScrollNav showUp showDown onUp={vi.fn()} onDown={vi.fn()} />,
    );
    expect(screen.getByRole("button", { name: /scroll to top/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /scroll to bottom/i })).toBeInTheDocument();
  });
});

describe("ScrollNav — callbacks", () => {
  it("calls onUp when the ↑ button is clicked", async () => {
    const onUp = vi.fn();
    const user = userEvent.setup();
    render(
      <ScrollNav showUp showDown={false} onUp={onUp} onDown={vi.fn()} />,
    );
    await user.click(screen.getByRole("button", { name: /scroll to top/i }));
    expect(onUp).toHaveBeenCalledOnce();
  });

  it("calls onDown when the ↓ button is clicked", async () => {
    const onDown = vi.fn();
    const user = userEvent.setup();
    render(
      <ScrollNav showUp={false} showDown onUp={vi.fn()} onDown={onDown} />,
    );
    await user.click(screen.getByRole("button", { name: /scroll to bottom/i }));
    expect(onDown).toHaveBeenCalledOnce();
  });
});
