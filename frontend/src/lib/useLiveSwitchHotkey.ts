import * as React from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import type { LiveSession } from "./types";

/** How long the `Ctrl-A` prefix stays armed waiting for a digit. */
const PREFIX_TIMEOUT_MS = 2000;

/** A regular form field where the browser's native `Ctrl-A` (select-all) wins. */
function isPlainFormField(el: Element | null): boolean {
  if (!el) return false;
  const tag = el.tagName;
  if (tag === "INPUT") return true;
  // The reply composer is a <textarea>; the xterm input proxy is too, but it
  // carries the `xterm-helper-textarea` class — there `Ctrl-A` is the
  // multiplexer prefix (tmux semantics), not select-all.
  if (tag === "TEXTAREA")
    return !el.classList.contains("xterm-helper-textarea");
  if (el instanceof HTMLElement && el.isContentEditable) return true;
  return false;
}

interface LiveSwitchHotkeyOptions {
  /** Transport-specific navigation; remote sessions use the default route. */
  onSelect?: (id: string) => void;
  /**
   * Invoked on `Ctrl-A c` — ask the CLI host to spawn a brand-new live session
   * (tmux's "new window"). When provided, the prefix arms even with a single
   * live session (you can always create another); host-presence gating and
   * toasts are the callback's own concern.
   */
  onCreate?: () => void;
}

/**
 * tmux-style `Ctrl-A` prefix hotkeys for the web, mirroring the CLI
 * multiplexer's prefix. Press `Ctrl-A` then:
 *   - a digit `1`–`9` → jump to that live session (1-indexed over the same list
 *     the tabs render);
 *   - `c` → create a new live session (via `onCreate`).
 * The prefix is intercepted on the document in the capture phase so it pre-empts
 * the live terminal's keystroke capture — `Ctrl-A` reaches the multiplexer, not
 * the remote `claude`.
 *
 * Inert unless there's something to do (≥2 sessions to switch between, or an
 * `onCreate` to make one), and deliberately ignored while a regular
 * input/textarea is focused so the reply composer keeps native select-all.
 * Switching preserves the `?fullscreen=1` flag, so a fullscreen switch stays
 * fullscreen.
 *
 * Returns whether the prefix is currently armed, for an on-screen hint.
 */
export function useLiveSwitchHotkey(
  sessions: Pick<LiveSession, "id">[],
  currentId: string,
  opts: LiveSwitchHotkeyOptions = {},
): { armed: boolean } {
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();

  // Mounted-once listener reads live data through refs so it never goes stale
  // and never needs re-registering (which could drop the prefix mid-chord).
  const sessionsRef = React.useRef(sessions);
  sessionsRef.current = sessions;
  const currentIdRef = React.useRef(currentId);
  currentIdRef.current = currentId;
  const fullscreenRef = React.useRef(false);
  fullscreenRef.current = searchParams.get("fullscreen") === "1";
  const onSelectRef = React.useRef(opts.onSelect);
  onSelectRef.current = opts.onSelect;
  const onCreateRef = React.useRef(opts.onCreate);
  onCreateRef.current = opts.onCreate;

  const [armed, setArmed] = React.useState(false);
  const armedRef = React.useRef(false);
  const setArmedBoth = React.useCallback((v: boolean) => {
    armedRef.current = v;
    setArmed(v);
  }, []);

  React.useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const disarm = () => {
      setArmedBoth(false);
      if (timer) clearTimeout(timer);
      timer = undefined;
    };

    const onKey = (e: KeyboardEvent) => {
      const list = sessionsRef.current;

      // While armed, the next key picks a session, creates one, or cancels.
      if (armedRef.current) {
        if (/^[1-9]$/.test(e.key)) {
          e.preventDefault();
          e.stopPropagation();
          const target = list[Number(e.key) - 1];
          disarm();
          if (target && target.id !== currentIdRef.current) {
            const search = fullscreenRef.current ? "?fullscreen=1" : "";
            if (onSelectRef.current) onSelectRef.current(target.id);
            else
              navigate(`/sessions/${encodeURIComponent(target.id)}${search}`);
          }
          return;
        }
        if ((e.key === "c" || e.key === "C") && onCreateRef.current) {
          e.preventDefault();
          e.stopPropagation();
          disarm();
          onCreateRef.current();
          return;
        }
        // Any other key cancels the prefix and is otherwise left alone.
        disarm();
        return;
      }

      // Nothing to do ⇒ don't claim Ctrl-A at all (no sessions to switch
      // between AND no create action).
      if (list.length < 2 && !onCreateRef.current) return;
      // Let regular form fields keep native select-all.
      if (isPlainFormField(document.activeElement)) return;

      if (
        e.ctrlKey &&
        !e.metaKey &&
        !e.altKey &&
        (e.key === "a" || e.key === "A")
      ) {
        e.preventDefault();
        e.stopPropagation();
        setArmedBoth(true);
        if (timer) clearTimeout(timer);
        timer = setTimeout(disarm, PREFIX_TIMEOUT_MS);
      }
    };

    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      if (timer) clearTimeout(timer);
    };
  }, [navigate, setArmedBoth]);

  return { armed };
}
