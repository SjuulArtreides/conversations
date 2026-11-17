//! Keep background diagnostics out of an active live terminal's scrollback.
use std::fmt;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const MESSAGE_LIFETIME: Duration = Duration::from_secs(10);
static STATUS: Mutex<Option<Status>> = Mutex::new(None);

#[derive(Default)]
struct Status {
    latest: Option<(String, Instant)>,
}

impl Status {
    fn record(&mut self, message: String) {
        self.latest = Some((message, Instant::now()));
    }

    fn line(&self, cols: usize) -> Option<String> {
        let (message, received) = self.latest.as_ref()?;
        (received.elapsed() < MESSAGE_LIFETIME).then(|| clip_line(message, cols))
    }
}

/// Declared before the runtime so background tasks stop before stderr is restored.
pub(crate) struct StatusGuard;

impl StatusGuard {
    pub(crate) fn enter() -> Self {
        if std::io::stdout().is_terminal() {
            *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(Status::default());
        }
        Self
    }
}

impl Drop for StatusGuard {
    fn drop(&mut self) {
        *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub(crate) fn report(args: fmt::Arguments<'_>, stderr: bool) {
    let mut status = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(status) = status.as_mut() {
        status.record(args.to_string());
    } else if stderr {
        eprintln!("{args}");
    } else {
        println!("{args}");
    }
}

pub(crate) fn has_message() -> bool {
    let mut guard = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(status) = guard.as_mut() else {
        return false;
    };
    let Some((_, received)) = status.latest.as_ref() else {
        return false;
    };
    if received.elapsed() >= MESSAGE_LIFETIME {
        status.latest = None;
    }
    // One final repaint restores the child list when the message expires.
    true
}

pub(crate) fn status_line(cols: usize) -> Option<String> {
    STATUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|status| status.line(cols))
}

// Reserve the final cell to avoid terminal autowrap. Never forward terminal
// controls from multiline HTTP errors or subprocess stderr into the status bar.
fn clip_line(message: &str, cols: usize) -> String {
    let mut remaining = cols.saturating_sub(1);
    message
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take_while(|ch| {
            let width = ch.width().unwrap_or(0);
            if width > remaining {
                return false;
            }
            remaining -= width;
            true
        })
        .collect()
}

pub(crate) fn frame(line: &str, rows: u16) -> String {
    format!("\x1b7\x1b[{};1H\x1b[2K\x1b[92m{}\x1b[0m\x1b8", rows, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_errors_replace_only_the_bottom_row_and_preserve_cursor() {
        let mut terminal = vt100::Parser::new(4, 20, 0);
        terminal.process(b"agent output\r\ninput");
        let cursor = terminal.screen().cursor_position();
        let mut status = Status::default();
        for message in [
            "first failure",
            "new\r\nerror\x1b[2J with a long explanation",
            "offline",
        ] {
            status.record(message.to_owned());
            terminal.process(frame(&status.line(20).unwrap(), 4).as_bytes());
            assert_eq!(terminal.screen().cursor_position(), cursor);
            assert_eq!(
                terminal.screen().contents().lines().next(),
                Some("agent output")
            );
        }
        assert_eq!(
            terminal.screen().contents(),
            "agent output\ninput\n\noffline"
        );
    }

    #[test]
    fn clips_wide_text_without_wrapping_even_on_tiny_terminals() {
        assert_eq!(clip_line("界界界", 6), "界界");
        assert_eq!(clip_line("error", 0), "");
        assert_eq!(clip_line("error", 1), "");
        assert_eq!(clip_line("a\nb\rc\td", 8), "a b c d");
    }

    #[test]
    fn restores_session_bar_after_message_expires() {
        let status = Status {
            latest: Some(("offline".into(), Instant::now() - MESSAGE_LIFETIME)),
        };
        assert_eq!(status.line(80), None);
    }
}
