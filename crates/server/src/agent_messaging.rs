//! Owner-bound messaging API. Participation does not grant terminal or transcript access.

use axum::{
    extract::{Path, State},
    Json,
};
use conversations_core::messaging::{AgentPeer, ConversationSummary};
use serde_json::{json, Value};

use crate::{
    error::{AppError, Result},
    identity::CurrentUser,
    AppState,
};

async fn owned(state: &AppState, viewer: Option<&str>, id: &str) -> Result<()> {
    let owner = state
        .repo
        .get_session_owner(id)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(viewer) = viewer {
        if owner.as_deref() != Some(viewer) {
            return Err(AppError::Forbidden(
                "only the session owner can use its messaging identity".into(),
            ));
        }
    }
    Ok(())
}

async fn active(state: &AppState, viewer: Option<&str>, id: &str) -> Result<AgentPeer> {
    owned(state, viewer, id).await?;
    state
        .repo
        .agent_messaging_context(id)
        .await?
        .ok_or_else(|| AppError::Forbidden("session does not exist".into()))
}

pub(crate) async fn peers(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let context = active(&state, viewer.as_deref(), &id).await?;
    let peers = state.repo.agent_messaging_peers(&id).await?;
    let mut sessions = Vec::with_capacity(peers.len());
    for peer in peers {
        let mut session = json!(peer);
        session["connected"] = json!(connected(&state, &peer));
        sessions.push(session);
    }
    Ok(Json(
        json!({"project_id":context.project_id, "sessions":sessions}),
    ))
}

/// Whether anything is centrally attached to this peer's session. Always
/// false: attachment is a fact of the peer's own host and its private
/// channels, which the server cannot observe. Kept as a wire field so older
/// readers keep parsing; do not read presence from it.
fn connected(_state: &AppState, peer: &AgentPeer) -> bool {
    let _ = peer;
    false
}

/// `live`: the session is registered live; `ended`: no live registration.
/// Nothing stronger can be claimed centrally (see [`connected`]).
fn peer_state(_connected: bool, peer: &AgentPeer) -> &'static str {
    match peer.live_status.as_deref() {
        Some("live") => "live",
        _ => "ended",
    }
}

/// Owner-only rail model: authorized peers plus message metadata for the ones
/// this session has actually exchanged messages with. Includes first-line
/// message titles and never records receipt. `readable` reports whether the viewer may
/// open the peer's transcript; messaging authority alone never sets it.
pub(crate) async fn communications(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let context = active(&state, viewer.as_deref(), &id).await?;
    let mut peers = state.repo.agent_messaging_peers(&id).await?;
    let summaries = state.repo.agent_conversation_summaries(&id).await?;
    // The directory is capped; a conversation partner beyond the cap is still
    // authorized (summaries apply current authorization) and must be listed.
    for (peer_id, _) in &summaries {
        if !peers.iter().any(|p| &p.session_id == peer_id) {
            if let Some(peer) = state.repo.agent_messaging_context(peer_id).await? {
                peers.push(peer);
            }
        }
    }
    let ids: Vec<String> = peers.iter().map(|p| p.session_id.clone()).collect();
    let readable = match viewer.as_deref() {
        Some(viewer) => Some(state.repo.readable_session_ids(viewer, &ids).await?),
        None => None,
    };
    let row = |peer: &AgentPeer, conversation: Option<&ConversationSummary>| {
        let state_name = peer_state(connected(&state, peer), peer);
        json!({
            "session_id": peer.session_id,
            "title": peer.title,
            "agent": peer.agent,
            "owner_email": peer.owner_email,
            "last_seen_at": peer.last_seen_at,
            "connected": connected(&state, peer),
            "state": state_name,
            // Can a message sent now be received? An ended session of the
            // viewer's own reopens to the same id; someone else's does not.
            "reachable": state_name != "ended" || peer.owner_email == context.owner_email,
            "readable": readable.as_ref().is_none_or(|r| r.contains(&peer.session_id)),
            "conversation": conversation,
        })
    };
    let mut sessions = Vec::with_capacity(peers.len());
    for (peer_id, summary) in &summaries {
        if let Some(peer) = peers.iter().find(|p| &p.session_id == peer_id) {
            sessions.push(row(peer, Some(summary)));
        }
    }
    for peer in &peers {
        if !summaries
            .iter()
            .any(|(peer_id, _)| peer_id == &peer.session_id)
        {
            sessions.push(row(peer, None));
        }
    }
    Ok(Json(json!({
        "project_id": context.project_id,
        "owner_email": context.owner_email,
        "sessions": sessions,
    })))
}
