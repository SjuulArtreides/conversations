//! Local launch requests: what the host, `start-agent` and their callers ask
//! the shared launch builder to start.

use conversations_core::{AgentKind, AgentLauncher};

/// A local launch request built by the host from a verified signed operation
/// or an owner-local handoff. This type is not a network protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    /// An idempotent owner-local handoff.
    Handoff {
        session_id: String,
        source_session_id: String,
        brief: String,
        agent: AgentKind,
        agent_args: Vec<String>,
        launcher: AgentLauncher,
        adk_statusline: bool,
        /// The model the child must start on; `None` keeps the agent's default.
        model: Option<conversations_core::messaging::HandoffModel>,
    },

    /// Resume a session using its locally recorded working directory and identity.
    Reopen {
        session_id: String,
        cwd: String,
        agent: AgentKind,
        /// The agent's NATIVE resume target, when it differs from `session_id`
        /// (Codex). `None` for Claude / legacy / uploaded rows → resume by
        /// `session_id`. `session_id` still keys the re-opened live session so
        /// the stored row continues across reopens.
        resume_id: Option<String>,
        /// Launch arguments resolved from local settings.
        agent_args: Vec<String>,
        /// How to invoke the agent.
        launcher: AgentLauncher,
        /// Whether an `adk code` child may draw its tmux status strip.
        adk_statusline: bool,
    },
    /// Start a session using the host's local launch settings.
    StartNew {
        cwd: Option<String>,
        agent: AgentKind,
        /// Launch arguments resolved from local settings.
        agent_args: Vec<String>,
        /// How to invoke the agent.
        launcher: AgentLauncher,
        /// Whether an `adk code` child may draw its tmux status strip.
        adk_statusline: bool,
    },
}
