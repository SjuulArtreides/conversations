//! Shared terminal history normalization for the CLI and server.
use crate::terminal::{partial_prefix_len, CODEX_TRANSCRIPT_REBUILD};

const SCROLLBACK_ROWS: usize = 2000;
const SYNC_START: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const MAX_REBUILD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyEraseEvent {
    PreserveBeforeErase,
    PreserveBeforeEraseFromHome,
    SuppressScrollbackPurge,
}

#[derive(Default)]
pub struct PtyEraseTracker {
    pending: Vec<u8>,
    pub replaced: bool,
    rebuild: Option<Vec<u8>>,
    top_region: Option<u16>,
}

impl PtyEraseTracker {
    /// Commit an interrupted redraw on host EOF, preserving its old prefix.
    pub fn finish(&mut self, parser: &mut vt100::Parser) -> Vec<u8> {
        if self.rebuild.is_some() {
            self.continue_rebuild(parser, SYNC_END)
        } else {
            Vec::new()
        }
    }

    fn continue_rebuild(&mut self, parser: &mut vt100::Parser, bytes: &[u8]) -> Vec<u8> {
        let buffered = self.rebuild.as_mut().expect("active rebuild");
        let search_from = buffered.len().saturating_sub(SYNC_END.len() - 1);
        buffered.extend_from_slice(bytes);
        // Codex writes the reset separately, then one synchronized update. Do
        // not treat PTY read boundaries as redraw completion boundaries.
        let end = buffered[search_from..]
            .windows(SYNC_END.len())
            .position(|w| w == SYNC_END)
            .map(|at| search_from + at + SYNC_END.len());
        let synchronized = buffered.starts_with(SYNC_START) || SYNC_START.starts_with(buffered);
        if end.is_none() && synchronized && buffered.len() < MAX_REBUILD_BYTES {
            return Vec::new();
        }
        let data = self.rebuild.take().unwrap();
        let boundary = end.unwrap_or_else(|| complete_prefix_len(&data));
        let (rows, cols) = parser.screen().size();
        let mut candidate = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        let final_region = process_codex_rebuild(&mut candidate, &data[..boundary]);
        let mut old = retained_rows(parser);
        while old.last().is_some_and(|row| row.text.is_empty()) {
            old.pop();
        }
        let mut fresh = retained_rows(&mut candidate);
        let header_len = startup_prefix_len(&fresh);
        // Recognized startup chrome is mutable UI. Submitted questions and
        // unknown rows remain candidates for exact transcript reconciliation.
        if header_len > 0 {
            if startup_only(&old) {
                old.clear();
            } else {
                let prefix = startup_prefix_len(&old);
                old.drain(..prefix);
                // The idle input and model footer describe the current UI.
                // They are not part of the submitted transcript to reconcile.
                if let Some(prompt) = old
                    .iter()
                    .rposition(|r| r.text.trim() == "› Ask Codex to do anything")
                {
                    if old[prompt + 1..].iter().all(startup_chrome_row) {
                        old.truncate(prompt);
                    }
                }
                while old.last().is_some_and(|r| r.text.trim().is_empty()) {
                    old.pop();
                }
            }
        }
        let header: Vec<_> = fresh.drain(..header_len).collect();
        // A full redraw can recover an earlier question before output already
        // retained from a clipped screen. Exact contiguous containment proves
        // that all old rows survive; appending them would reorder/duplicate them.
        let contained = !old.is_empty()
            && fresh.windows(old.len()).any(|window| window == old)
            && old.iter().any(|row| !row.text.trim().is_empty());
        if contained {
            old.clear();
        }
        self.top_region = final_region;
        // Contiguous overlap only: global row/set deduplication would erase
        // legitimate repeated answers and repeated lines within an answer.
        let overlap = (1..=old.len().min(fresh.len()))
            .rev()
            .find(|&n| {
                old[old.len() - n..] == fresh[..n]
                    && fresh[..n].iter().any(|row| !row.text.is_empty())
            })
            .unwrap_or(0);
        let mut merged = b"\x1b[r\x1b[H\x1b[2J\x1b[3J".to_vec();
        let prefix = &old[..old.len() - overlap];
        for row in header.iter().chain(prefix.iter()).chain(fresh.iter()) {
            merged.extend_from_slice(&row.formatted);
            merged.extend_from_slice(b"\x1b[m\r\n");
        }
        // No final newline: it would scroll the candidate's viewport by a row.
        merged.truncate(merged.len() - b"\x1b[m\r\n".len());
        if let Some(bottom) = final_region {
            merged.extend_from_slice(format!("\x1b[1;{bottom}r").as_bytes());
        }
        merged.extend(candidate.screen().cursor_state_formatted());
        merged.extend(candidate.screen().attributes_formatted());
        merged.extend(candidate.screen().input_mode_formatted());
        *parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        parser.process(&merged);
        self.replaced = true;
        // Preserve output after the boundary, including another rebuild in the
        // same read. A later replacement supersedes the earlier snapshot.
        if boundary < data.len() {
            let tail = self.process(parser, &data[boundary..]);
            if self.replaced {
                return tail;
            }
            merged.extend(tail);
            self.replaced = true;
        }
        merged
    }

    /// Fold raw child output into the retained terminal parser and return the
    /// bytes safe to mirror to the browser.
    ///
    /// An application `CSI 3 J` is suppressed: honoring that purge destroys the
    /// history this parser exists to retain, and forwarding it makes xterm erase
    /// its own scrollback too. The preceding display clear still reaches both
    /// parsers, and our synthetic keyframes keep their self-clearing prefix
    /// because they bypass this raw-output tracker.
    ///
    /// Codex rebuilds are buffered through their synchronized-update boundary.
    /// Idle startup chrome is replaced. Exact contiguous overlap/containment
    /// reconciles transcript rows; omitted or changed answers remain retained.
    pub fn process(&mut self, parser: &mut vt100::Parser, bytes: &[u8]) -> Vec<u8> {
        self.replaced = false;
        if self.rebuild.is_some() {
            return self.continue_rebuild(parser, bytes);
        }
        if bytes.is_empty() {
            return Vec::new();
        }

        let mut combined = Vec::new();
        let input = if self.pending.is_empty() {
            bytes
        } else {
            combined.extend_from_slice(&self.pending);
            combined.extend_from_slice(bytes);
            self.pending.clear();
            combined.as_slice()
        };

        // Hold back a trailing partial signature so the next chunk can complete
        // it; see `partial_prefix_len`. Without this the `ESC[2J` half would
        // archive the screen before the `ESC[3J` half arrived to retire it.
        let held = partial_prefix_len(input, CODEX_TRANSCRIPT_REBUILD).max(utf8_tail_len(input));
        let (input, held_tail) = input.split_at(input.len() - held);

        let mut scan = 0usize;
        let mut processed = 0usize;
        let mut mirrored = Vec::with_capacity(input.len());
        while scan < input.len() {
            if input[scan..].starts_with(CODEX_TRANSCRIPT_REBUILD) {
                if scan > processed {
                    mirrored.extend(self.process_output(parser, &input[processed..scan]));
                }
                self.rebuild = Some(Vec::new());
                let remaining =
                    [&input[scan + CODEX_TRANSCRIPT_REBUILD.len()..], held_tail].concat();
                let replacement = self.continue_rebuild(parser, &remaining);
                if self.replaced {
                    return replacement;
                }
                mirrored.extend_from_slice(&replacement);
                return mirrored;
            }
            match csi_sequence_at(input, scan) {
                CsiScan::NotCsi => {
                    scan += 1;
                }
                CsiScan::Incomplete => {
                    if scan > processed {
                        mirrored.extend(self.process_output(parser, &input[processed..scan]));
                    }
                    self.pending.extend_from_slice(&input[scan..]);
                    self.pending.extend_from_slice(held_tail);
                    return mirrored;
                }
                CsiScan::Complete { end, event } => {
                    if input[end - 1] == b'r' {
                        mirrored.extend(self.process_output(parser, &input[processed..scan]));
                        if !parser.screen().alternate_screen() {
                            self.top_region =
                                top_region(&input[scan..end], parser.screen().size().0);
                        }
                        parser.process(&input[scan..end]);
                        mirrored.extend_from_slice(&input[scan..end]);
                        processed = end;
                    } else if input[end - 1] == b'S'
                        && self.top_region.is_some()
                        && csi_params(&input[scan..end])
                            .bytes()
                            .all(|b| b.is_ascii_digit())
                    {
                        mirrored.extend(self.process_output(parser, &input[processed..scan]));
                        let count = csi_params(&input[scan..end])
                            .parse::<u16>()
                            .unwrap_or(1)
                            .max(1);
                        mirrored.extend(self.scroll_region(parser, &input[scan..end], count));
                        processed = end;
                    } else if let Some(event) = event {
                        if scan > processed {
                            mirrored.extend(self.process_output(parser, &input[processed..scan]));
                        }
                        match event {
                            PtyEraseEvent::PreserveBeforeErase => {
                                preserve_visible_screen_in_scrollback(parser);
                                parser.process(&input[scan..end]);
                                mirrored.extend_from_slice(&input[scan..end]);
                            }
                            PtyEraseEvent::PreserveBeforeEraseFromHome => {
                                preserve_visible_screen_in_scrollback_from_home(parser);
                                parser.process(&input[scan..end]);
                                mirrored.extend_from_slice(&input[scan..end]);
                            }
                            PtyEraseEvent::SuppressScrollbackPurge => {}
                        }
                        processed = end;
                    }
                    scan = end;
                }
            }
        }

        if processed < input.len() {
            mirrored.extend(self.process_output(parser, &input[processed..]));
        }
        self.pending.extend_from_slice(held_tail);
        mirrored
    }

    fn process_output(&mut self, parser: &mut vt100::Parser, bytes: &[u8]) -> Vec<u8> {
        if self.top_region.is_none() || parser.screen().alternate_screen() {
            parser.process(bytes);
            return bytes.to_vec();
        }
        let mut out = Vec::new();
        let mut start = 0;
        for (at, &byte) in bytes.iter().enumerate() {
            if matches!(byte, b'\n' | 0x0b | 0x0c) {
                parser.process(&bytes[start..at]);
                out.extend_from_slice(&bytes[start..at]);
                if self.top_region == Some(parser.screen().cursor_position().0 + 1) {
                    out.extend(self.scroll_region(parser, &bytes[at..at + 1], 1));
                } else {
                    parser.process(&bytes[at..at + 1]);
                    out.push(byte);
                }
                start = at + 1;
            }
        }
        parser.process(&bytes[start..]);
        out.extend_from_slice(&bytes[start..]);
        out
    }

    /// Model the real partial-region operation, then archive the same removed
    /// rows with a full-height scroll and repair only the visible differences.
    /// No temporary dimensions or full display erase reach the viewer.
    fn scroll_region(&self, parser: &mut vt100::Parser, operation: &[u8], count: u16) -> Vec<u8> {
        let Some(bottom) = self
            .top_region
            .filter(|_| !parser.screen().alternate_screen())
        else {
            parser.process(operation);
            return operation.to_vec();
        };
        let (rows, cols) = parser.screen().size();
        if bottom >= rows {
            parser.process(operation);
            return operation.to_vec();
        }
        let mut candidate = vt100::Parser::new(rows, cols, 0);
        candidate.process(&parser.screen().contents_formatted());
        candidate.process(format!("\x1b[1;{bottom}r").as_bytes());
        candidate.process(&parser.screen().cursor_state_formatted());
        candidate.process(&parser.screen().attributes_formatted());
        candidate.process(operation);
        let mut out = format!("\x1b[r\x1b[{rows};1H").into_bytes();
        out.extend(std::iter::repeat_n(b'\n', usize::from(count.min(bottom))));
        parser.process(&out);
        let patch = candidate.screen().contents_diff(parser.screen());
        parser.process(&patch);
        out.extend(patch);
        let mut restore = format!("\x1b[1;{bottom}r").into_bytes();
        restore.extend(candidate.screen().cursor_state_formatted());
        restore.extend(candidate.screen().attributes_formatted());
        parser.process(&restore);
        out.extend(restore);
        out
    }
}

#[derive(PartialEq, Eq)]
struct RetainedRow {
    text: String,
    formatted: Vec<u8>,
}

fn csi_params(bytes: &[u8]) -> &str {
    let start = if bytes[0] == 0x9b { 1 } else { 2 };
    std::str::from_utf8(&bytes[start..bytes.len() - 1]).unwrap_or("")
}

fn top_region(bytes: &[u8], rows: u16) -> Option<u16> {
    let mut parts = csi_params(bytes).split(';');
    let top = parts.next()?.parse::<u16>().ok()?;
    let bottom = parts.next()?.parse::<u16>().ok()?;
    (top == 1 && bottom > 1 && bottom < rows && parts.next().is_none()).then_some(bottom)
}

fn startup_chrome_row(row: &RetainedRow) -> bool {
    let text = row.text.trim();
    text.is_empty()
        || text.starts_with(['╭', '╰', '│'])
        || text.starts_with(">_ OpenAI Codex")
        || text.starts_with("~/")
        || text.starts_with("permissions:")
        || text.starts_with("Tip:")
        || text == "› Ask Codex to do anything"
        || text == "? for shortcuts"
        || ((text.starts_with("gpt-") || text.starts_with("GPT-")) && text.contains(" · "))
        || (text.starts_with("⚠ ") && text.contains("MCP startup issue"))
        || (text.starts_with("• You have ") && text.contains("usage limit reset"))
}

fn startup_prefix_len(rows: &[RetainedRow]) -> usize {
    let end = rows
        .iter()
        .position(|r| !startup_chrome_row(r))
        .unwrap_or(rows.len());
    if rows[..end]
        .iter()
        .any(|r| r.text.contains(">_ OpenAI Codex (v"))
    {
        end
    } else {
        0
    }
}

fn startup_only(rows: &[RetainedRow]) -> bool {
    startup_prefix_len(rows) == rows.len()
        && rows
            .iter()
            .any(|r| r.text.trim() == "› Ask Codex to do anything")
}

/// Codex prints its rebuilt transcript into a top-aligned scroll region, with
/// the prompt below it. vt100 only retains rows scrolled through a FULL-height
/// region. Temporarily model that transcript region at its own height, keeping
/// the lower screen aside, then restore the real geometry before publishing.
/// This is confined to a completed redraw; no temporary size reaches a viewer.
fn process_codex_rebuild(parser: &mut vt100::Parser, bytes: &[u8]) -> Option<u16> {
    let (rows, cols) = parser.screen().size();
    let mut lower: Option<(u16, Vec<Vec<u8>>)> = None;
    let mut scan = 0;
    let mut processed = 0;
    while scan < bytes.len() {
        match csi_sequence_at(bytes, scan) {
            CsiScan::Complete { end, .. } if bytes[end - 1] == b'r' => {
                parser.process(&bytes[processed..scan]);
                restore_lower_screen(parser, rows, cols, lower.take());
                let body = if bytes[scan] == 0x9b {
                    scan + 1
                } else {
                    scan + 2
                };
                let params = std::str::from_utf8(&bytes[body..end - 1]).unwrap_or("");
                let mut parts = params.split(';');
                let top = parts
                    .next()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(1);
                let bottom = parts
                    .next()
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap_or(rows);
                if top == 1 && bottom > 1 && bottom < rows && parts.next().is_none() {
                    let saved = parser
                        .screen()
                        .rows_formatted(0, cols)
                        .skip(usize::from(bottom))
                        .collect();
                    lower = Some((bottom, saved));
                    parser.screen_mut().set_size(bottom, cols);
                }
                parser.process(&bytes[scan..end]);
                processed = end;
                scan = end;
            }
            CsiScan::Complete { end, .. } => scan = end,
            CsiScan::Incomplete => break,
            CsiScan::NotCsi => scan += 1,
        }
    }
    parser.process(&bytes[processed..]);
    let final_region = lower.as_ref().map(|(bottom, _)| *bottom);
    restore_lower_screen(parser, rows, cols, lower);
    final_region
}

fn restore_lower_screen(
    parser: &mut vt100::Parser,
    rows: u16,
    cols: u16,
    lower: Option<(u16, Vec<Vec<u8>>)>,
) {
    let Some((bottom, saved)) = lower else {
        return;
    };
    let cursor = parser.screen().cursor_state_formatted();
    let attrs = parser.screen().attributes_formatted();
    parser.screen_mut().set_size(rows, cols);
    for (index, row) in saved.iter().enumerate() {
        parser.process(format!("\x1b[{};1H\x1b[m", usize::from(bottom) + index + 1).as_bytes());
        parser.process(row);
    }
    parser.process(&cursor);
    parser.process(&attrs);
}

fn retained_rows(parser: &mut vt100::Parser) -> Vec<RetainedRow> {
    let (rows, cols) = parser.screen().size();
    parser.screen_mut().set_scrollback(usize::MAX);
    let total = parser.screen().scrollback();
    let mut result = Vec::with_capacity(total + usize::from(rows));
    for offset in (1..=total).rev() {
        parser.screen_mut().set_scrollback(offset);
        result.push(RetainedRow {
            text: parser.screen().rows(0, cols).next().unwrap_or_default(),
            formatted: parser
                .screen()
                .rows_formatted(0, cols)
                .next()
                .unwrap_or_default(),
        });
    }
    parser.screen_mut().set_scrollback(0);
    result.extend(
        parser
            .screen()
            .rows(0, cols)
            .zip(parser.screen().rows_formatted(0, cols))
            .map(|(text, formatted)| RetainedRow { text, formatted }),
    );
    result
}

// A conservative unsynchronized redraw must not strand an unfinished escape
// or UTF-8 character in the temporary parser when we construct its snapshot.
fn complete_prefix_len(bytes: &[u8]) -> usize {
    let mut scan = 0;
    while scan < bytes.len() {
        match csi_sequence_at(bytes, scan) {
            CsiScan::Incomplete => return scan,
            CsiScan::Complete { end, .. } => scan = end,
            CsiScan::NotCsi => scan += 1,
        }
    }
    bytes.len() - utf8_tail_len(bytes)
}

fn utf8_tail_len(bytes: &[u8]) -> usize {
    let Some(start) = bytes.iter().rposition(|byte| byte & 0xc0 != 0x80) else {
        return 0;
    };
    let width = match bytes[start] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return 0,
    };
    let available = bytes.len() - start;
    if available < width {
        available
    } else {
        0
    }
}

enum CsiScan {
    NotCsi,
    Incomplete,
    Complete {
        end: usize,
        event: Option<PtyEraseEvent>,
    },
}

fn csi_sequence_at(input: &[u8], start: usize) -> CsiScan {
    let Some(&first) = input.get(start) else {
        return CsiScan::NotCsi;
    };
    let params_start = match first {
        b'\x1b' => {
            let Some(&next) = input.get(start + 1) else {
                return CsiScan::Incomplete;
            };
            if next != b'[' {
                return CsiScan::NotCsi;
            }
            start + 2
        }
        0x9b => start + 1,
        _ => return CsiScan::NotCsi,
    };

    let mut i = params_start;
    while i < input.len() {
        let b = input[i];
        if (0x40..=0x7e).contains(&b) {
            let event = (b == b'J')
                .then(|| erase_display_event(&input[params_start..i]))
                .flatten();
            return CsiScan::Complete { end: i + 1, event };
        }
        i += 1;
    }

    CsiScan::Incomplete
}

fn erase_display_event(params: &[u8]) -> Option<PtyEraseEvent> {
    // Private DECSED (`CSI ? 2 J`) gets the same treatment as ED (`CSI 2 J`):
    // both clear visible display memory in terminals that support them.
    let params = params.strip_prefix(b"?").unwrap_or(params);
    if params.iter().any(|b| (0x20..=0x2f).contains(b)) {
        return None;
    }
    let first = params
        .split(|b| *b == b';' || *b == b':')
        .next()
        .unwrap_or_default();
    let mode = if first.is_empty() {
        0
    } else {
        let s = std::str::from_utf8(first).ok()?;
        s.parse::<u16>().ok()?
    };
    match mode {
        0 => Some(PtyEraseEvent::PreserveBeforeEraseFromHome),
        2 => Some(PtyEraseEvent::PreserveBeforeErase),
        3 => Some(PtyEraseEvent::SuppressScrollbackPurge),
        _ => None,
    }
}

fn preserve_visible_screen_in_scrollback_from_home(parser: &mut vt100::Parser) {
    let at_home = parser.screen().cursor_position() == (0, 0);
    if at_home {
        preserve_visible_screen_in_scrollback(parser);
    }
}

fn preserve_visible_screen_in_scrollback(parser: &mut vt100::Parser) {
    let screen = parser.screen();
    if screen.alternate_screen() {
        return;
    }
    let (_, cols) = screen.size();
    let count = screen
        .rows(0, cols)
        .enumerate()
        .filter(|(_, row)| !row.is_empty())
        .map(|(row, _)| row + 1)
        .last()
        .unwrap_or(0);
    if count > 0 {
        // Preserve the existing rows, attributes, and wrap flags without moving
        // the cursor or rewriting cells. A home+rewrite only scrolls one row.
        parser.process(format!("\x1b[{count}S").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc8_oversized_target_clears_previous_link_without_replaying_it() {
        for (parameters, uri) in [
            (
                String::new(),
                format!("https://example.com/{}", "x".repeat(1024 * 1024)),
            ),
            (
                "id=boundary".to_owned(),
                "x".repeat(4096 - "id=boundary".len()),
            ),
            ("x".repeat(4096), "https://example.com/short".to_owned()),
        ] {
            let mut parser = vt100::Parser::new(3, 80, 0);
            parser.process(b"\x1b]8;;https://example.com/previous\x1b\\");
            parser.process(format!("\x1b]8;{parameters};{uri}\x1b\\label").as_bytes());
            assert_eq!(parser.screen().contents(), "label");
            assert!(parser.screen().cell(0, 0).unwrap().hyperlink().is_none());
            let replay = parser.screen().contents_formatted();
            assert!(replay.len() < 100, "oversized target survived in replay");
        }
    }

    #[test]
    fn osc8_target_at_combined_byte_limit_remains_clickable_after_replay() {
        let parameters = "id=boundary";
        let uri = format!(
            "https://example.com/{}",
            "x".repeat(4096 - parameters.len() - 1 - "https://example.com/".len())
        );
        let mut parser = vt100::Parser::new(3, 80, 0);
        parser.process(format!("\x1b]8;{parameters};{uri}\x1b\\label\x1b]8;;\x1b\\").as_bytes());
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().hyperlink(),
            Some(uri.as_str())
        );
        let mut viewer = vt100::Parser::new(3, 80, 0);
        viewer.process(&parser.screen().contents_formatted());
        assert_eq!(
            viewer.screen().cell(0, 0).unwrap().hyperlink(),
            Some(uri.as_str())
        );
    }

    #[test]
    fn osc8_parameter_limit_never_replays_a_truncated_or_previous_target() {
        let mut parser = vt100::Parser::new(3, 80, 0);
        parser.process(b"\x1b]8;;https://example.com/previous\x1b\\");
        let oversized = format!(
            "\x1b]8;;https://example.com/{}\x1b\\label",
            (0..20).map(|i| i.to_string()).collect::<Vec<_>>().join(";")
        );
        parser.process(oversized.as_bytes());
        assert_eq!(parser.screen().contents(), "label");
        assert!(parser.screen().cell(0, 0).unwrap().hyperlink().is_none());
        let replay = String::from_utf8(parser.screen().contents_formatted()).unwrap();
        assert!(!replay.contains("https://example.com/"));
    }

    #[test]
    fn osc8_wide_characters_preserve_targets_after_row_replay() {
        let mut parser = vt100::Parser::new(3, 8, 0);
        parser.process("\x1b]8;;https://example.com/wide\x1b\\界界界界界\x1b]8;;\x1b\\".as_bytes());
        let mut viewer = vt100::Parser::new(3, 8, 0);
        viewer.process(&parser.screen().contents_formatted());
        assert_eq!(viewer.screen().contents(), parser.screen().contents());
        for (row, col) in [(0, 0), (0, 2), (0, 4), (0, 6), (1, 0)] {
            assert_eq!(
                viewer.screen().cell(row, col).unwrap().hyperlink(),
                Some("https://example.com/wide")
            );
        }
        viewer.process(b"\x1b[H ");
        assert!(viewer.screen().cell(0, 0).unwrap().hyperlink().is_none());
        assert!(viewer.screen().cell(0, 1).unwrap().hyperlink().is_none());
    }

    #[test]
    fn formatted_replay_clears_stale_links_and_restores_active_target() {
        let mut source = vt100::Parser::new(3, 20, 0);
        source.process(b"plain \x1b]8;;https://example.com/active\x1b\\A");
        let row = source.screen().rows_formatted(0, 20).next().unwrap();
        for replay in [row, source.screen().contents_formatted()] {
            let mut viewer = vt100::Parser::new(3, 20, 0);
            viewer.process(b"\x1b]8;;https://example.com/stale\x1b\\");
            viewer.process(&replay);
            assert!(viewer.screen().cell(0, 0).unwrap().hyperlink().is_none());
            assert_eq!(
                viewer.screen().cell(0, 6).unwrap().hyperlink(),
                Some("https://example.com/active")
            );
        }
        let mut viewer = vt100::Parser::new(3, 20, 0);
        viewer.process(&source.screen().contents_formatted());
        viewer.process(b"B");
        assert_eq!(
            viewer.screen().cell(0, 7).unwrap().hyperlink(),
            Some("https://example.com/active")
        );
    }

    #[test]
    fn osc8_cells_follow_terminal_mutations_and_split_sequences() {
        let bytes = b"\x1b]8;id=first;https://example.com/one;a\x1b\\same\x1b[0m!\x1b]8;;\x1b\\ \x1b]8;;https://example.com/two\x07same\x1b]8;;\x07";
        for split in 0..=bytes.len() {
            let mut parser = vt100::Parser::new(3, 8, 8);
            parser.process(&bytes[..split]);
            parser.process(&bytes[split..]);
            assert_eq!(
                parser.screen().cell(0, 4).unwrap().hyperlink(),
                Some("https://example.com/one;a")
            );
            assert_eq!(
                parser.screen().cell(0, 6).unwrap().hyperlink(),
                Some("https://example.com/two")
            );
            assert_eq!(
                parser.screen().cell(1, 0).unwrap().hyperlink(),
                Some("https://example.com/two")
            );
            let rows: Vec<_> = parser.screen().rows_formatted(0, 8).collect();
            let mut viewer = vt100::Parser::new(3, 8, 8);
            for (index, row) in rows.iter().enumerate() {
                viewer.process(format!("\x1b[{};1H", index + 1).as_bytes());
                viewer.process(row);
            }
            assert_eq!(
                viewer.screen().cell(0, 4).unwrap().hyperlink(),
                Some("https://example.com/one;a")
            );
            assert_eq!(
                viewer.screen().cell(1, 0).unwrap().hyperlink(),
                Some("https://example.com/two")
            );
            parser.process(b"\x1b[Hplain\x1b[K");
            assert!(parser.screen().cell(0, 0).unwrap().hyperlink().is_none());
            assert!(parser.screen().cell(0, 6).unwrap().hyperlink().is_none());
            parser.process(b"\x1b[3;1H\r\n");
            parser.screen_mut().set_scrollback(usize::MAX);
            assert_eq!(
                parser.screen().cell(1, 0).unwrap().hyperlink(),
                Some("https://example.com/two")
            );
        }
    }

    #[test]
    fn osc8_targets_survive_codex_rebuild_with_identical_labels() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        let links = b"\x1b]8;;https://example.com/one\x1b\\PR #216\x1b]8;;\x1b\\ \x1b]8;;https://example.com/two\x07PR #216\x1b]8;;\x07";
        let mut redraw = CODEX_TRANSCRIPT_REBUILD.to_vec();
        redraw.extend_from_slice(SYNC_START);
        redraw.extend_from_slice(links);
        redraw.extend_from_slice(SYNC_END);
        let replay = tracker.process(&mut parser, &redraw);
        let replay = String::from_utf8(replay).unwrap();
        assert!(replay.contains("https://example.com/one"), "{replay:?}");
        assert!(replay.contains("https://example.com/two"), "{replay:?}");
        assert_eq!(parser.screen().contents().matches("PR #216").count(), 2);
    }

    fn text(parser: &mut vt100::Parser) -> Vec<String> {
        retained_rows(parser)
            .into_iter()
            .map(|r| r.text)
            .filter(|r| !r.is_empty())
            .collect()
    }

    fn redraw(body: &[u8]) -> Vec<u8> {
        [CODEX_TRANSCRIPT_REBUILD, SYNC_START, body, SYNC_END].concat()
    }

    #[test]
    fn codex_startup_rebuild_replaces_chrome_before_the_first_question() {
        let mut parser = vt100::Parser::new(12, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(
            &mut parser,
            b"  >_ OpenAI Codex (v0.161.0)\r\n     ~/example\r\n\r\n",
        );
        tracker.process(
            &mut parser,
            "› Ask Codex to do anything\r\n  GPT-6.1-Sol default · ~/example".as_bytes(),
        );
        tracker.process(&mut parser, &redraw("  >_ OpenAI Codex (v0.161.0)\r\n     ~/example\r\n  permissions: YOLO mode\r\n› original question\r\nanswer".as_bytes()));
        let lines = text(&mut parser);
        assert_eq!(
            lines.iter().filter(|s| s.contains("OpenAI Codex")).count(),
            1
        );
        assert_eq!(
            lines
                .iter()
                .filter(|s| s.contains("original question"))
                .count(),
            1
        );
        assert!(!lines.iter().any(|s| s.contains("Ask Codex")));
    }

    #[test]
    fn recovered_full_transcript_places_question_before_already_retained_output() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(&mut parser, b"answer detail\r\nanswer conclusion");
        tracker.process(
            &mut parser,
            &redraw(
                "› original question\r\nanswer heading\r\nanswer detail\r\nanswer conclusion"
                    .as_bytes(),
            ),
        );
        assert_eq!(
            text(&mut parser),
            [
                "› original question",
                "answer heading",
                "answer detail",
                "answer conclusion"
            ]
        );
    }

    #[test]
    fn full_redraw_removes_stale_chrome_without_losing_submitted_questions() {
        let mut parser = vt100::Parser::new(12, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(&mut parser, "  >_ OpenAI Codex (v0.161.0)\r\n     ~/example\r\n› first question\r\nold answer\r\n› Ask Codex to do anything\r\n  GPT-6.1-Sol default · ~/example".as_bytes());
        tracker.process(&mut parser, &redraw("  >_ OpenAI Codex (v0.161.0)\r\n     ~/example\r\n› first question\r\nold answer\r\n› second question\r\nnew answer".as_bytes()));
        assert_eq!(
            text(&mut parser),
            [
                "  >_ OpenAI Codex (v0.161.0)",
                "     ~/example",
                "› first question",
                "old answer",
                "› second question",
                "new answer"
            ]
        );
        tracker.process(&mut parser, &redraw("  >_ OpenAI Codex (v0.161.0)\r\n     ~/example\r\n› second question\r\nchanged answer".as_bytes()));
        let lines = text(&mut parser);
        assert!(lines.iter().any(|r| r == "› first question"));
        assert!(lines.iter().any(|r| r == "old answer"));
        assert!(lines.iter().any(|r| r == "new answer"));
        assert!(lines.iter().any(|r| r == "changed answer"));
        assert_eq!(
            lines.iter().filter(|r| r.contains("OpenAI Codex")).count(),
            1
        );
    }

    #[test]
    fn ordinary_transcript_region_output_retains_the_question_and_prompt() {
        let bytes = b"\x1b[6;1Hinput\x1b[1;3rquestion\r\nanswer 1\r\nanswer 2\r\nanswer 3\r\nanswer 4\x1b[r\x1b[6;1Hinput";
        for split in 0..=bytes.len() {
            let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
            let mut viewer = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
            let mut tracker = PtyEraseTracker::default();
            for chunk in [&bytes[..split], &bytes[split..]] {
                let mirrored = tracker.process(&mut parser, chunk);
                viewer.process(&mirrored);
            }
            assert_eq!(
                text(&mut parser),
                ["question", "answer 1", "answer 2", "answer 3", "answer 4", "input"],
                "split {split}"
            );
            assert_eq!(text(&mut viewer), text(&mut parser), "viewer split {split}");
            assert_eq!(parser.screen().cursor_position(), (5, 5));
        }
    }

    #[test]
    fn explicit_region_scroll_preserves_styled_unicode_and_cursor() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut viewer = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        for chunk in [
            "\x1b[1;3r\x1b[31mquestion 日本語\r\nanswer\r\nlast\x1b[2;4H".as_bytes(),
            b"\x1b[2S",
        ] {
            viewer.process(&tracker.process(&mut parser, chunk));
        }
        assert_eq!(text(&mut parser), ["question 日本語", "answer", "last"]);
        assert_eq!(text(&mut viewer), text(&mut parser));
        assert_eq!(parser.screen().cursor_position(), (1, 3));
        assert_eq!(parser.screen().fgcolor(), vt100::Color::Idx(1));
        assert_eq!(viewer.screen().fgcolor(), parser.screen().fgcolor());
    }

    #[test]
    fn alternate_screen_region_output_does_not_enter_conversation_history() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(
            &mut parser,
            b"normal question\x1b[1;3r\x1b[?1049h\x1b[1;2rtemporary\r\nmore\r\nlast\x1b[?1049l",
        );
        assert_eq!(text(&mut parser), ["normal question"]);
        tracker.process(&mut parser, b"\x1b[3;1Hanswer\r\nnext");
        assert!(text(&mut parser).iter().any(|r| r == "normal question"));
    }

    #[test]
    fn identical_overlap_is_removed_but_omitted_and_changed_text_survives() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(&mut parser, b"old answer\r\ndetail\r\nlatest answer");
        tracker.process(&mut parser, &redraw(b"latest answer\r\nnew answer"));
        assert_eq!(
            text(&mut parser),
            ["old answer", "detail", "latest answer", "new answer"]
        );
        tracker.process(&mut parser, &redraw(b"new answer"));
        assert_eq!(
            text(&mut parser),
            ["old answer", "detail", "latest answer", "new answer"]
        );
        tracker.process(&mut parser, &redraw(b"corrected answer"));
        assert_eq!(
            text(&mut parser),
            [
                "old answer",
                "detail",
                "latest answer",
                "new answer",
                "corrected answer"
            ]
        );
    }

    #[test]
    fn redraw_is_independent_of_every_possible_two_chunk_split() {
        let bytes = redraw(b"overlap\r\nnew answer");
        for split in 0..=bytes.len() {
            let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
            let mut tracker = PtyEraseTracker::default();
            tracker.process(&mut parser, b"old answer\r\noverlap");
            tracker.process(&mut parser, &bytes[..split]);
            tracker.process(&mut parser, &bytes[split..]);
            assert_eq!(
                text(&mut parser),
                ["old answer", "overlap", "new answer"],
                "split {split}"
            );
        }
    }

    #[test]
    fn redraw_waits_for_completion_without_publishing_a_blank_screen() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(&mut parser, b"old answer");
        let start = [CODEX_TRANSCRIPT_REBUILD, SYNC_START, b"new answer"].concat();
        assert!(tracker.process(&mut parser, &start).is_empty());
        assert_eq!(text(&mut parser), ["old answer"]);
        tracker.finish(&mut parser);
        assert_eq!(text(&mut parser), ["old answer", "new answer"]);
    }

    #[test]
    fn repeated_lines_inside_a_new_answer_are_not_globally_deduplicated() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(&mut parser, b"same\r\nsame\r\noverlap");
        tracker.process(&mut parser, &redraw(b"overlap\r\nsame\r\nsame"));
        assert_eq!(
            text(&mut parser),
            ["same", "same", "overlap", "same", "same"]
        );
    }

    #[test]
    fn codex_rebuild_retains_lines_scrolled_through_its_transcript_region() {
        let mut parser = vt100::Parser::new(6, 80, SCROLLBACK_ROWS);
        let mut tracker = PtyEraseTracker::default();
        tracker.process(&mut parser, b"earlier answer");
        tracker.process(
            &mut parser,
            &redraw(b"\x1b[1;3r1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\x1b[r\x1b[6;1Hprompt"),
        );
        assert_eq!(
            text(&mut parser),
            [
                "earlier answer",
                "1",
                "2",
                "3",
                "4",
                "5",
                "6",
                "7",
                "prompt"
            ]
        );
        assert_eq!(parser.screen().contents(), "5\n6\n7\n\n\nprompt");
    }
}
