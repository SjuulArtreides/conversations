//! Local-owner file drops use the same wrapper permits and receipts as private transfers.
use super::super::{
    protocol::TransferContent,
    transfers::{self, TransferItem, TransferKind},
};
use super::*;
use conversations_core::{
    private_channel::TransferStatus,
    terminal_files::{
        validate_file_name, IMAGE_MIMES, MAX_FILES, MAX_IMAGE_BYTES, MAX_TOTAL_BYTES,
    },
};

/// Validate bytes before allocating a permit; the wrapper independently checks these exact hashes.
pub(crate) fn describe(content: &TransferContent) -> Result<TransferKind> {
    match content {
        TransferContent::Image { mime, b64 } => {
            ensure!(
                IMAGE_MIMES.contains(&mime.as_str()),
                "unsupported image type"
            );
            ensure!(
                b64.len() <= MAX_IMAGE_BYTES.div_ceil(3) * 4,
                "image too large"
            );
            let bytes = STANDARD.decode(b64).context("invalid image encoding")?;
            ensure!(
                !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES,
                "invalid image size"
            );
            Ok(TransferKind::Image {
                mime: mime.clone(),
                size: bytes.len() as u64,
                sha256: transfers::sha256_hex(&bytes),
            })
        }
        TransferContent::Files { files } => {
            ensure!(
                !files.is_empty() && files.len() <= MAX_FILES,
                "invalid file count"
            );
            let mut total = 0;
            let mut items = Vec::with_capacity(files.len());
            for file in files {
                ensure!(
                    validate_file_name(&file.name).is_ok()
                        && !file.name.contains(['\u{2028}', '\u{2029}']),
                    "invalid file name"
                );
                ensure!(
                    file.b64.len() <= MAX_TOTAL_BYTES.div_ceil(3) * 4,
                    "files too large"
                );
                let bytes = STANDARD
                    .decode(&file.b64)
                    .context("invalid file encoding")?;
                total += bytes.len();
                ensure!(total <= MAX_TOTAL_BYTES, "files too large");
                items.push(TransferItem {
                    name: file.name.clone(),
                    size: bytes.len() as u64,
                    sha256: transfers::sha256_hex(&bytes),
                });
            }
            Ok(TransferKind::Files { files: items })
        }
    }
}

pub(super) async fn send(
    host: &Host,
    session: &str,
    transfer: &str,
    content: TransferContent,
) -> TransferStatus {
    send_with_wait(
        host,
        session,
        transfer,
        content,
        transfers::AUTH_TTL + Duration::from_secs(30),
    )
    .await
}
async fn send_with_wait(
    host: &Host,
    session: &str,
    transfer: &str,
    content: TransferContent,
    wait: Duration,
) -> TransferStatus {
    if !authz::envelope::valid_session_id(session)
        || !(16..=64).contains(&transfer.len())
        || !transfer.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return TransferStatus::Refused;
    }
    let Ok(kind) = describe(&content) else {
        return TransferStatus::Refused;
    };
    let connection = {
        let sessions = host.sessions.lock().unwrap();
        let Some(live) = sessions.get(session) else {
            return TransferStatus::Failed;
        };
        if !live.registration.file_receiver
            || !live.registration.transfer_gate
            || !live.registration.effect_gate
        {
            return TransferStatus::Refused;
        }
        let Some(screen) = &live.screen else {
            return TransferStatus::Failed;
        };
        Arc::downgrade(screen)
    };
    // This is a quota bucket, not an enrolled credential or signed authority.
    if host
        .transfers
        .reserve("local-owner", session, transfer, kind.bytes())
        .is_err()
    {
        return TransferStatus::Refused;
    }
    struct Reserved<'a>(&'a transfers::TransferLedger, &'a str);
    impl Drop for Reserved<'_> {
        fn drop(&mut self) {
            self.0.release(self.1);
        }
    }
    let _reserved = Reserved(&host.transfers, transfer);
    let Some(receipt) = private_channel::expect_receipt(&host.dir, session, &connection, transfer)
    else {
        return TransferStatus::Refused;
    };
    let authorization = ServerMessage::TransferAuthorized {
        transfer: transfer.into(),
        kind,
        expires_unix_ms: transfers::deadline_in(transfers::AUTH_TTL),
        authority: None,
    };
    let content = ServerMessage::TransferContent {
        transfer: transfer.into(),
        content,
    };
    // Both enqueue checks bind to this exact wrapper connection, including across reconnects.
    if private_channel::enqueue_on(&host.sessions, session, &connection, authorization).is_err()
        || private_channel::enqueue_on(&host.sessions, session, &connection, content).is_err()
    {
        private_channel::forget_receipt(&host.dir, transfer);
        return TransferStatus::Failed;
    }
    private_channel::outcome(&host.dir, transfer, receipt, wait).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversations_core::terminal_files::{TerminalFile, TerminalFileStatus};
    const SESSION: &str = "00000000-0000-0000-0000-000000000001";
    const TRANSFER: &str = "0123456789abcdef0123456789abcdef";
    fn attached() -> (
        tempfile::TempDir,
        Arc<Host>,
        super::super::super::protocol::SharedScreen,
        mpsc::Receiver<ServerMessage>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let fixture = super::super::authz_tests::Fixture::new(dir.path());
        let host = Arc::new(fixture.host_without_cmux());
        let screen = Screen::new(80, 24);
        let (commands, rx) = mpsc::channel(4);
        host.sessions.lock().unwrap().insert(
            SESSION.into(),
            Session {
                screen: Some(screen.clone()),
                readiness: Default::default(),
                local_input_bytes: 0,
                local_input_dropped_chunks: 0,
                registration: Registration {
                    launch: Launch {
                        id: SESSION.into(),
                        agent: conversations_core::AgentKind::Claude,
                        cwd: "/work".into(),
                        command: vec!["claude".into()],
                    },
                    workspace: None,
                    surface: None,
                    local_terminal: true,
                    reconnecting: false,
                    file_receiver: true,
                    transfer_gate: true,
                    effect_gate: true,
                },
                resume_published: None,
                opencode_title_published: None,
                commands,
                pid: None,
            },
        );
        (dir, host, screen, rx)
    }
    fn files() -> TransferContent {
        TransferContent::Files {
            files: vec![TerminalFile {
                name: "Résumé 日本語.txt".into(),
                b64: STANDARD.encode(b"exact bytes"),
            }],
        }
    }
    #[tokio::test]
    async fn owner_transfer_uses_exact_wrapper_hash_permit_and_receipt() {
        let (_dir, host, screen, mut rx) = attached();
        let task_host = host.clone();
        let task = tokio::spawn(async move { send(&task_host, SESSION, TRANSFER, files()).await });
        let table = transfers::TransferTable::default();
        let ServerMessage::TransferAuthorized {
            transfer,
            kind,
            expires_unix_ms,
            authority,
        } = rx.recv().await.unwrap()
        else {
            panic!("missing authorization")
        };
        assert!(authority.is_none());
        table
            .authorize_as(&transfer, kind.clone(), expires_unix_ms, authority)
            .unwrap();
        assert!(
            table
                .authorize_as(&transfer, kind, expires_unix_ms, None)
                .is_err(),
            "replay permit must be refused"
        );
        let ServerMessage::TransferContent {
            content: TransferContent::Files { files },
            ..
        } = rx.recv().await.unwrap()
        else {
            panic!("missing content")
        };
        let mut wrong = files.clone();
        wrong[0].b64 = STANDARD.encode(b"substituted");
        assert!(table.claim_files(TRANSFER, &wrong).is_none());
        let claim = table.claim_files(TRANSFER, &files).unwrap();
        assert!(claim.permit.denial().is_none());
        assert!(table.claim_files(TRANSFER, &files).is_none());
        private_channel::transfer_receipt(
            &host.dir,
            SESSION,
            &screen,
            TRANSFER,
            TerminalFileStatus::Sent,
        );
        assert_eq!(task.await.unwrap(), TransferStatus::Delivered);
    }
    #[tokio::test]
    async fn owner_transfer_never_accepts_replacement_receipt_and_reports_uncertainty() {
        let (_dir, host, _screen, mut rx) = attached();
        let task_host = host.clone();
        let task = tokio::spawn(async move {
            send_with_wait(
                &task_host,
                SESSION,
                TRANSFER,
                files(),
                Duration::from_millis(40),
            )
            .await
        });
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        let replacement = Screen::new(80, 24);
        private_channel::transfer_receipt(
            &host.dir,
            SESSION,
            &replacement,
            TRANSFER,
            TerminalFileStatus::Sent,
        );
        assert_eq!(task.await.unwrap(), TransferStatus::Uncertain);
    }
    #[test]
    fn owner_transfer_rejects_invalid_names_types_and_sizes() {
        assert!(describe(&TransferContent::Image {
            mime: "text/html".into(),
            b64: STANDARD.encode(b"x")
        })
        .is_err());
        assert!(describe(&TransferContent::Files {
            files: vec![TerminalFile {
                name: "../escape".into(),
                b64: String::new()
            }]
        })
        .is_err());
        assert!(describe(&TransferContent::Image {
            mime: "image/png".into(),
            b64: STANDARD.encode(vec![0; MAX_IMAGE_BYTES + 1])
        })
        .is_err());
    }
}
