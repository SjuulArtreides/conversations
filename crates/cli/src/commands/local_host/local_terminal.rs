//! Direct owner-socket terminal attachment. This never enters the remote relay.
use super::super::protocol::LocalTerminalInput;
use super::*;
use std::sync::Weak;

pub(super) async fn attach(
    host: Arc<Host>,
    mut socket: tokio::net::UnixStream,
    id: String,
) -> Result<()> {
    let target = host.sessions.lock().unwrap().get(&id).and_then(|s| {
        s.screen
            .as_ref()
            .map(|screen| (s.commands.clone(), Arc::downgrade(screen)))
    });
    let Some((commands, screen)) = target else {
        return write_message(
            &mut socket,
            &ServerMessage::Error {
                message: "This terminal is no longer connected. Refresh the session list.".into(),
            },
        )
        .await;
    };
    follow(socket, commands, screen).await
}

async fn follow(
    socket: tokio::net::UnixStream,
    commands: mpsc::Sender<ServerMessage>,
    screen: Weak<std::sync::Mutex<super::super::protocol::Screen>>,
) -> Result<()> {
    let subscribe = || screen.upgrade().map(|s| s.lock().unwrap().subscribe());
    let Some((first, mut frames)) = subscribe() else {
        return Ok(());
    };
    let (mut reader, mut writer) = socket.into_split();
    write_message(&mut writer, &ServerMessage::TerminalFrame { frame: first }).await?;
    // read_message is not cancellation-safe. Keep one reader task while output streams.
    let (input_tx, mut input_rx) = mpsc::channel(16);
    let input_task = tokio::spawn(async move {
        while let Ok(input) = read_message::<_, LocalTerminalInput>(&mut reader).await {
            if input_tx.send(input).await.is_err() {
                break;
            }
        }
    });
    let result: Result<()> = async {
        loop {
            tokio::select! {
                input = input_rx.recv() => {
                    let Some(input) = input else { break; };
                    let command = match input {
                        LocalTerminalInput::Repaint => ServerMessage::Repaint,
                        LocalTerminalInput::Input { b64 } => {
                            ensure!(b64.len() <= 128 * 1024, "terminal input too large");
                            STANDARD.decode(&b64)?;
                            ServerMessage::Input { b64, authority: None }
                        }
                        LocalTerminalInput::Resize { cols, rows } => {
                            ensure!(cols > 0 && rows > 0 && cols <= 1000 && rows <= 1000, "invalid terminal dimensions");
                            ServerMessage::Resize { cols, rows }
                        }
                    };
                    // No buffered replay to a disconnected or replacement wrapper.
                    commands.try_send(command).context("terminal disconnected or busy")?;
                }
                frame = frames.recv() => {
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let Some((frame, fresh)) = subscribe() else { break; };
                            frames = fresh;
                            frame
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    write_message(&mut writer, &ServerMessage::TerminalFrame { frame }).await?;
                }
            }
        }
        Ok(())
    }.await;
    input_task.abort();
    result
}

pub(super) async fn action(
    host: &Host,
    action: super::super::protocol::LocalAction,
) -> Result<String> {
    use super::super::protocol::LocalAction;
    let id = match &action {
        LocalAction::End { session }
        | LocalAction::Start { session, .. }
        | LocalAction::Reopen { session }
        | LocalAction::Reply { session, .. } => session.clone(),
    };
    ensure!(authz::envelope::valid_session_id(&id), "invalid session id");
    match action {
        LocalAction::End { session } => {
            ensure!(
                host.sessions.lock().unwrap().contains_key(&session),
                "session is not running on this host"
            );
            host.journal_end(&session).await;
            authz::store::Store::open(&host.dir)?.close_for_restore(&session)?;
            host.deliver_end(&session);
        }

        LocalAction::Start {
            session,
            agent,
            launcher,
        } => {
            let settings = super::super::settings::Settings::load(&host.dir)?;
            let operation = authz::envelope::Operation::SessionStart {
                session,
                agent,
                launcher,
            };
            let (id, command) = authz::dispatch::start_command(&settings, &operation)?;
            ensure!(
                !host.sessions.lock().unwrap().contains_key(&id)
                    && authz::lookup(&host.dir, &id)?.is_none()
                    && !wrapper_alive(&host.dir, &id).await,
                "session already exists"
            );
            let launch = launch_from_control_with_id(
                command,
                &host.cwd,
                Some(&host.server),
                Some(id),
                &host.launch_env,
            )?;
            host.launch(&launch).await?;
        }
        LocalAction::Reopen { session: id } => {
            let store = authz::store::Store::open(&host.dir)?;
            ensure!(
                store.can_reopen_import(&id)?,
                "imported session is read-only"
            );
            let settings = super::super::settings::Settings::load(&host.dir)?;
            let session = authz::lookup(&host.dir, &id)?.context("unknown local session")?;
            reopen_prepared(host, store, settings, session).await?;
        }
        LocalAction::Reply { session, text } => {
            ensure!(text.len() <= 64 * 1024, "reply too large");
            let text = sanitize_reply(&text);
            ensure!(!text.trim().is_empty(), "message is empty after sanitizing");
            let commands = host
                .sessions
                .lock()
                .unwrap()
                .get(&session)
                .map(|s| s.commands.clone())
                .context("terminal is not connected")?;
            commands
                .try_send(ServerMessage::Input {
                    b64: STANDARD.encode(text),
                    authority: None,
                })
                .context("terminal busy or disconnected")?;
            tokio::time::sleep(Duration::from_millis(80)).await;
            commands
                .try_send(ServerMessage::Input {
                    b64: STANDARD.encode(b"\r"),
                    authority: None,
                })
                .context("terminal busy or disconnected")?;
        }
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_terminal_streams_snapshot_input_resize_and_disconnect() {
        let screen = super::super::super::protocol::Screen::new(80, 24);
        screen.lock().unwrap().output(b"initial prompt");
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (commands, mut received) = mpsc::channel(4);
        let task = tokio::spawn(follow(server, commands, Arc::downgrade(&screen)));
        let first: ServerMessage = read_message(&mut client).await.unwrap();
        let ServerMessage::TerminalFrame { frame } = first else {
            panic!("missing snapshot")
        };
        assert!(String::from_utf8(STANDARD.decode(frame.b64).unwrap())
            .unwrap()
            .contains("initial prompt"));
        let bytes = b"echo hello\r\x1b\x03";
        write_message(
            &mut client,
            &LocalTerminalInput::Input {
                b64: STANDARD.encode(bytes),
            },
        )
        .await
        .unwrap();
        match received.recv().await.unwrap() {
            ServerMessage::Input {
                b64,
                authority: None,
            } => assert_eq!(STANDARD.decode(b64).unwrap(), bytes),
            _ => panic!("wrong input"),
        }
        write_message(
            &mut client,
            &LocalTerminalInput::Resize {
                cols: 100,
                rows: 30,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            received.recv().await,
            Some(ServerMessage::Resize {
                cols: 100,
                rows: 30
            })
        ));
        write_message(&mut client, &LocalTerminalInput::Repaint)
            .await
            .unwrap();
        assert!(matches!(
            received.recv().await,
            Some(ServerMessage::Repaint)
        ));
        screen.lock().unwrap().output(b"hello back");
        let output: ServerMessage = read_message(&mut client).await.unwrap();
        let ServerMessage::TerminalFrame { frame } = output else {
            panic!("missing output")
        };
        assert_eq!(STANDARD.decode(frame.b64).unwrap(), b"hello back");
        drop(screen);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
