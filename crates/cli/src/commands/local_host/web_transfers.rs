//! Owner-authenticated loopback uploads, never a public-server relay.
use super::super::protocol::TransferContent;
use super::*;
use conversations_core::{
    private_channel::TransferStatus,
    terminal_files::{MAX_FILES, MAX_NAME_BYTES, MAX_TOTAL_BYTES},
};
pub(super) const MAX_UPLOAD_BODY: usize =
    4 * (MAX_TOTAL_BYTES / 3 + MAX_FILES) + MAX_FILES * MAX_NAME_BYTES * 6 + 64 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Upload {
    transfer: String,
    content: TransferContent,
}
pub(super) async fn upload(
    State(state): State<Arc<WebState>>,
    RoutePath(session): RoutePath<String>,
    Json(input): Json<Upload>,
) -> Response {
    if !authz::envelope::valid_session_id(&session)
        || !(16..=64).contains(&input.transfer.len())
        || !input.transfer.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return error(StatusCode::BAD_REQUEST, "Invalid transfer or session id.");
    }
    if let Err(e) = super::super::daemon::local_transfer::describe(&input.content) {
        return error(StatusCode::BAD_REQUEST, &e.to_string());
    }
    let result: Result<TransferStatus> = async {
        let mut socket = tokio::net::UnixStream::connect(state.dir.join("host.sock")).await?;
        write_message(
            &mut socket,
            &ClientMessage::LocalTransfer {
                session,
                transfer: input.transfer,
                content: input.content,
            },
        )
        .await?;
        match tokio::time::timeout(
            super::super::transfers::AUTH_TTL + std::time::Duration::from_secs(31),
            read_message(&mut socket),
        )
        .await??
        {
            ServerMessage::LocalTransferResult { status } => Ok(status),
            _ => Ok(TransferStatus::Uncertain),
        }
    }
    .await;
    Json(json!({"status":result.unwrap_or(TransferStatus::Uncertain)})).into_response()
}
