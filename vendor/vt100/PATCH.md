# Local vt100 patch

Source: vt100 0.16.2, https://crates.io/crates/vt100/0.16.2,
https://github.com/doy/vt100-rust. The source and MIT license are retained.
Crates.io archive SHA-256: `054ff75fb8fa83e609e685106df4faeffdf3a735d3c74ebce97ec557d5d36fd9`.
Only src, README, and LICENSE are copied; the small manifest preserves runtime dependencies.

Local changes retain OSC 8 parameters and URI with each printed cell and emit
hyperlink transitions from formatted row/screen replay. SGR resets keep hyperlink
state; erase clears it. Attributes are cloned instead of copied, sharing an Arc<String>
across cells within a link. There is no global URL table: the allocation is released
when its last cell or active/saved attribute is dropped. Cells increase from 32 to
48 bytes on 64-bit targets; the existing bounded scrollback limits cell retention.
Targets consume one shared allocation per OSC 8 opening sequence, rather than one
allocation per linked character. Replay rows close open links to prevent leakage.
OSC 8 sequences that reach vte's 16-parameter limit clear the active target,
because additional semicolon-separated URI segments may have been discarded.

Behavior is exercised in core terminal_history, CLI interactive mirror replay,
and server live snapshot/history compaction tests. Keep this patch when upgrading
vt100 until upstream provides equivalent OSC 8 cell and replay support.

OSC 8 parameters, their separator, and the complete URI have a combined limit of
4096 bytes. Longer targets clear the active link before allocating a joined URI
or retained target, preventing amplification when each retained row reopens a
link during snapshot replay. Invalid UTF-8 also clears the link rather than
changing its destination through replacement characters.

Control characters in parameters and targets are rejected. Regression tests also
cover the canonical vendor parser in crates/core/tests/terminal_links.rs.
