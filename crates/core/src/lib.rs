//! `conversations-core`: data model + agent transcript parser + filesystem
//! discovery. No async / I/O-heavy dependencies — this crate is shared by the
//! server and the CLI.
//!
//! # Robustness
//! Parsing a session never fails on an unknown record `type` (captured/ignored)
//! or a single malformed JSON line (skipped and counted in
//! [`Session::skipped_lines`]). Hard errors are limited to [`ParseError`]
//! (I/O, etc.).
//!
//! # Skipped-line reporting
//! The chosen design carries the count on the parsed value itself:
//! [`Session::skipped_lines`]. This keeps the parse entry points returning a
//! plain `Result<Session, ParseError>`, and the count travels with the session
//! over the wire (it is `Serialize`/`Deserialize`) so the server and CLI can
//! surface partial-parse warnings without a side-channel.

pub mod attention;
pub mod communications;
mod discovery;
pub mod host_relay;
pub mod linear;
pub mod messaging;
pub use linear::SessionLinearDoc;
mod model;
mod parser;
mod projects;
pub use projects::Project;
pub mod private_channel;
pub mod publication;
mod repos;
pub mod terminal;
pub mod terminal_files;
pub mod terminal_history;

pub use discovery::{
    claude_projects_dir, codex_sessions_dir, decode_project_dir, discover_sessions,
    discover_sessions_for_agent,
};
pub use model::{
    AgentKind, AgentLauncher, ContentBlock, Message, MessageKind, RepoLink, Role, Session,
    SessionFileRef, SessionSummary, Usage,
};
pub use parser::{
    blocks_from_raw_message, codex_user_message_title, parse_block, parse_line,
    parse_opencode_export, parse_opencode_export_str, parse_session_file, parse_session_str,
    quick_summary, ParseError,
};
pub use repos::touched_paths;

pub mod migration;
pub mod terminal_prs;
