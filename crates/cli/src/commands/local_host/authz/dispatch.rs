//! Admission pipeline between a received signed frame and an execution adapter.
//! The daemon calls [`decide`] for every signed frame and performs an effect only
//! for `Decision::Execute`; the effect is bracketed by `begin`/`finish` so the
//! journal always records how far an operation got.

use std::path::Path;

use anyhow::Result;
use conversations_core::AgentLauncher;

use super::envelope::{verify_as, Operation, SignedFrame, Verified};
use super::store::{Admission, JournalState, Store};
use crate::commands::live::host::ControlCommand;
use crate::commands::local_host::settings::Settings;

#[derive(Debug)]
pub enum Decision {
    /// Verified, authorized and journaled as `admitted`: perform the effect once.
    Execute(Verified),
    /// Already seen. Report the recorded state; never execute again.
    Replay {
        state: String,
        /// Recorded outcome. Deliberately not relayed: it may name local details.
        #[allow(dead_code)]
        result: Option<String>,
    },
    /// An input-class frame (keystrokes, resize, repaint): authorized against
    /// current rows just now, NOT journaled. The caller orders it per channel and
    /// performs the effect once.
    Stream(Verified),
    /// Verified, authorized and already applied inside the admission transaction
    /// (a local authority change). Nothing further may run.
    Applied(&'static str),
    /// Not authorized. Nothing was journaled, nothing may run.
    Refused(String),
}

pub fn decide(dir: &Path, frame: &SignedFrame, now: i64) -> Result<Decision> {
    let mut store = Store::open(dir)?;
    let host_id = store.host_id()?;
    // Which stored key to try comes from the (still untrusted) claimed
    // delegation id; the verified envelope must repeat exactly that value.
    let Some(dlg) = super::envelope::claimed_delegation(&frame.payload) else {
        return Ok(Decision::Refused("malformed operation".into()));
    };
    let Some((alg, public_key)) = store.signing_key(&frame.kid, dlg.as_deref(), now)? else {
        return Ok(Decision::Refused(
            "unknown, revoked or expired credential or delegation".into(),
        ));
    };
    let mut verified = match verify_as(frame, alg, &public_key, &host_id, now, dlg.as_deref()) {
        Ok(verified) => verified,
        Err(reject) => return Ok(Decision::Refused(reject.to_string())),
    };
    if verified.op.is_stream() {
        return Ok(match store.authorize_stream(&verified, now)? {
            Ok(epoch) => {
                verified.grant_epoch = epoch;
                Decision::Stream(verified)
            }
            Err(reason) => Decision::Refused(reason.into()),
        });
    }
    Ok(match store.admit(&mut verified, now)? {
        Admission::New { epoch } => {
            verified.grant_epoch = epoch;
            Decision::Execute(verified)
        }
        Admission::Applied(detail) => Decision::Applied(detail),
        Admission::Duplicate { state, result } => Decision::Replay { state, result },
        Admission::PayloadMismatch => {
            Decision::Refused("operation id reused with different content".into())
        }
        Admission::Denied(reason) => Decision::Refused(reason.into()),
    })
}

/// Re-verify `frame` and check its signer's CURRENT authority, for an answer
/// that carries content (a private read), new or replayed. Same verifier and
/// rules as [`decide`]; nothing is journaled. Returns the refusal reason, if any.
pub fn current_authority(dir: &Path, frame: &SignedFrame, now: i64) -> Result<Option<String>> {
    let mut store = Store::open(dir)?;
    let host_id = store.host_id()?;
    let Some(dlg) = super::envelope::claimed_delegation(&frame.payload) else {
        return Ok(Some("malformed operation".into()));
    };
    let Some((alg, public_key)) = store.signing_key(&frame.kid, dlg.as_deref(), now)? else {
        return Ok(Some(
            "unknown, revoked or expired credential or delegation".into(),
        ));
    };
    let verified = match verify_as(frame, alg, &public_key, &host_id, now, dlg.as_deref()) {
        Ok(verified) => verified,
        Err(reject) => return Ok(Some(reject.to_string())),
    };
    Ok(store.current_authority(&verified, now)?.map(str::to_owned))
}

/// The effect is about to start: from here a crash leaves the operation
/// `uncertain` (never retried) rather than `failed` (safe to re-sign). The
/// authority the operation was admitted under is re-checked in the same
/// transaction; a revocation committed since admission fails it with
/// [`Revoked`] and nothing runs.
pub fn begin(dir: &Path, op: &Verified) -> Result<()> {
    match Store::open(dir)?.begin(op, super::store::now_secs())? {
        None => Ok(()),
        Some(reason) => Err(anyhow::Error::new(Revoked(reason))),
    }
}

/// The operation's authority was revoked between admission and its effect.
#[derive(Debug)]
pub struct Revoked(pub &'static str);

impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "authority was revoked before the effect: {}", self.0)
    }
}

impl std::error::Error for Revoked {}

pub fn is_revoked(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Revoked>().is_some()
}

/// The effect may have taken place even though the call reported failure (a child
/// was spawned, or an RPC was sent, and nothing confirmed the outcome). The
/// journal records `uncertain`, never `failed`: no blind duplicate is admitted
/// and local state tied to the possible effect is kept.
#[derive(Debug)]
pub struct EffectUncertain(pub String);

impl std::fmt::Display for EffectUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "outcome unknown: {}", self.0)
    }
}

impl std::error::Error for EffectUncertain {}

pub fn is_uncertain(error: &anyhow::Error) -> bool {
    error.downcast_ref::<EffectUncertain>().is_some()
}

/// The error to hand to [`finish`] for a failed effect, keeping the uncertainty
/// classification of `error` while dropping its (host-local) detail chain.
pub fn outcome_error(error: &anyhow::Error) -> anyhow::Error {
    if is_uncertain(error) {
        anyhow::Error::new(EffectUncertain(format!("{error:#}")))
    } else {
        anyhow::anyhow!("{error:#}")
    }
}

/// Record the outcome of an effect that [`begin`] opened. An entry that was
/// settled meanwhile (a start its wrapper confirmed while the launch wait was
/// giving up) keeps its state.
pub fn finish(dir: &Path, op: &Verified, outcome: &Result<()>) -> Result<()> {
    let mut store = Store::open(dir)?;
    match outcome {
        Ok(()) => store.mark(
            &op.kid,
            &op.jti,
            JournalState::Done,
            Some(match op.op {
                Operation::SessionReopen { .. } => "reopened",
                Operation::SessionReply { .. } => "replied",
                Operation::SessionRename { .. } => "renamed",
                Operation::SessionEnd { .. } => "ended",
                Operation::SessionPasteImage { .. } | Operation::SessionSendFiles { .. } => {
                    "authorized"
                }
                Operation::AgentMessage { .. } => "queued",
                Operation::AgentStatus { .. } => "status",
                Operation::PrSync { .. } => "synced",
                Operation::PrUnlink { .. } => "unlinked",
                Operation::PrComment { .. } => "commented",
                Operation::PublicationShare { .. } => "sharing",
                Operation::PublicationUnshare { .. } => "unsharing",
                Operation::PublicationStatus { .. } => "status",
                Operation::SettingsGet
                | Operation::HistoryGet { .. }
                | Operation::PrCacheList { .. }
                | Operation::HistoryFragment { .. }
                | Operation::TerminalSubscribe { .. }
                | Operation::SessionsList { .. }
                | Operation::HistorySearch { .. } => "read",
                Operation::SettingsUpdate { .. } => "updated",
                Operation::MachinePair { .. } => "paired",
                Operation::MachineUnpair => "unpaired",
                _ => "started",
            }),
        ),
        Err(error) if is_uncertain(error) => store.mark(
            &op.kid,
            &op.jti,
            JournalState::Uncertain,
            Some(&format!("{error:#}")),
        ),
        Err(error) => store.mark(
            &op.kid,
            &op.jti,
            JournalState::Failed,
            Some(&format!("{error:#}")),
        ),
    }
}

/// Build the launch command for a verified start from LOCAL settings only.
/// The signed operation carries just the agent and an optional launcher; launch
/// arguments, harness choice and permission modes come from this host's config.
pub fn start_command(settings: &Settings, op: &Operation) -> Result<(String, ControlCommand)> {
    let Operation::SessionStart {
        session,
        agent,
        launcher,
    } = op
    else {
        anyhow::bail!("not a session start");
    };
    let launch = &settings.launch;
    let launcher = match launcher {
        Some(launcher) => *launcher,
        None if launch.default_launcher.supports(*agent) => launch.default_launcher,
        None => AgentLauncher::default(),
    };
    let statusline = launcher == AgentLauncher::Adk && launch.adk_statusline;
    Ok((
        session.clone(),
        ControlCommand::StartNew {
            cwd: None,
            agent: *agent,
            agent_args: launch.agent_args(*agent)?,
            launcher,
            adk_statusline: statusline,
        },
    ))
}

/// Build the resume command for a verified reopen entirely from local state:
/// the local session record (cwd, native resume id, harness) and this host's
/// current launch settings. Nothing here comes from the frame but the session id.
pub fn reopen_command(
    settings: &Settings,
    session: &super::sessions::LocalSession,
) -> Result<ControlCommand> {
    // Codex and OpenCode resume only their own native id; a Conversations id is
    // rejected by those CLIs, and guessing one would start the wrong session.
    anyhow::ensure!(
        !matches!(
            session.agent,
            conversations_core::AgentKind::Codex | conversations_core::AgentKind::OpenCode
        ) || session.resume_id.is_some(),
        "agent has not reported its native session identity; nothing to resume"
    );
    Ok(ControlCommand::Reopen {
        session_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        agent: session.agent,
        resume_id: session.resume_id.clone(),
        agent_args: settings.launch.agent_args(session.agent)?,
        launcher: session.launcher,
        adk_statusline: session.adk_statusline,
    })
}
