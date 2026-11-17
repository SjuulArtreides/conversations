// Revocation cutover, observed as what a recording agent on a real PTY actually
// received. Two stubs: the raw recorder (`agent_body`, `cat`) is QUEUE/BYTE
// evidence only, what reached the terminal and that the same process survived;
// the stateful work stub (`work_stub`) models one active operation and is the
// evidence that a revocation interrupts running work without ending the agent.
// Every test drives the production host (the local socket and `session`, and
// signed operations through `deliver_signed`: for private operations (including
// agent messages, input, replies, transfers and grants), the dispatch the private channel makes once it has authenticated and
// decrypted a request; the channel itself is proved in
// `tests/private_transport.rs`), the
// production wrapper (`wrapper::run`) and a child
// process that records its terminal input and reports its PID. Barriers sit at
// real production boundaries: the host-to-wrapper socket (held by a transparent
// proxy), an acknowledged final-writer barrier, and the central transfer
// preflight. Revocation is committed between admission and the final effect.

use super::*;
use crate::commands::local_host::wrapper::writer_barrier::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Central stand-in: `204` for everything (mirror registration retries,
/// acknowledgements). The wrapper opens no central consumer streams any more —
/// clipboard/file drops travel the private channel — so there is nothing to
/// subscribe to here.
struct Central {
    url: String,
}

async fn central() -> Central {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..n]);
                }
                let _ = socket.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await;
            });
        }
    });
    Central { url }
}

/// A production host serving session A through the production wrapper, with a
/// transparent proxy on the host socket that can HOLD what the host queued for
/// the wrapper. Session B is a plain recorded queue (another, unaffected terminal).
struct Rig {
    w: World,
    host: Arc<Host>,
    current: Arc<Mutex<Arc<Host>>>,
    b: mpsc::Receiver<ServerMessage>,
    central: Central,
    hold: Arc<AtomicBool>,
    held: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    to_wrapper: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ServerMessage>>>>,
    links: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    connections: Arc<AtomicUsize>,
    wrapper: tokio::task::JoinHandle<Result<u32>>,
    agent: PathBuf,
}

/// Whether `pid` is still a running (not zombie) child of THIS test process.
/// Observation only: teardown never signals a PID read back from a fixture.
fn owned_child_running(pid: i32) -> bool {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-o", "ppid=,stat=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    let line = String::from_utf8_lossy(&out.stdout);
    let mut fields = line.split_whitespace();
    let ppid: Option<u32> = fields.next().and_then(|p| p.parse().ok());
    let stat = fields.next().unwrap_or("Z");
    ppid == Some(std::process::id()) && !stat.starts_with('Z')
}

/// Failure-path cleanup: also runs while a failed assertion unwinds. The rig
/// owns its fixtures' lifetime through their control file: it writes
/// `agent/stop`, which every fixture polls and answers with `agent.stopped`
/// before exiting (a PTY hang-up ends them too). Nothing is signalled; a PID a
/// fixture reported is evidence of survival, never cleanup authority. Then it
/// stops the wrapper and proxy tasks and waits a bounded time for the answer.
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::write(self.agent.join("stop"), b"");
        self.wrapper.abort();
        for link in self
            .links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            link.abort();
        }
        // The PID is read only to observe the fixture end (its own `stop`
        // answer, or the PTY hang-up from the aborted wrapper); never signalled.
        let Some(pid) = std::fs::read_to_string(self.agent.join("agent.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
        else {
            return;
        };
        for _ in 0..150 {
            if self.agent.join("agent.stopped").exists() || !owned_child_running(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("the agent fixture {pid} did not end within 3 s of its stop request");
    }
}

/// The raw recorder loop: every byte from the PTY, unchanged, appended to
/// `agent.rec` until `stop` (or hang-up); then `agent.stopped`.
const RECORDER: &str = r#"use strict; use warnings; use IO::Select;
my ($dir) = @ARGV;
open(my $out, '>>', "$dir/agent.rec") or die; binmode $out; $out->autoflush(1);
binmode STDIN;
my $select = IO::Select->new(\*STDIN);
while (!-e "$dir/stop") {
  next unless $select->can_read(0.05);
  my $n = sysread(STDIN, my $bytes, 4096);
  last unless $n;
  print $out $bytes;
}
open(my $done, '>', "$dir/agent.stopped"); close $done;
"#;

/// The raw recording agent (queue/byte evidence): report its PID, wait for
/// `go` (until then nothing reads the PTY, so writes back up), then record
/// every byte it is handed.
fn agent_body(agent: &Path) -> String {
    std::fs::write(agent.join("recorder.pl"), RECORDER).unwrap();
    format!(
        "echo $$ > '{d}/agent.pid'; /bin/stty raw -echo; : > '{d}/agent.rec'; while [ ! -e '{d}/go' ]; do if [ -e '{d}/stop' ]; then : > '{d}/agent.stopped'; exit 0; fi; /bin/sleep 0.02; done; exec /usr/bin/perl '{d}/recorder.pl' '{d}'",
        d = agent.display()
    )
}

fn host_for(w: &World, central: &Central) -> (Arc<Host>, mpsc::Receiver<ServerMessage>) {
    let mut host = w.fixture.host(central.url.clone());
    host.cmux = None;
    let (commands, b) = mpsc::channel(64);
    host.sessions.lock().unwrap().insert(
        B.into(),
        Session {
            screen: Some(Screen::new(80, 24)),
            readiness: Default::default(),
            local_input_bytes: 0,
            local_input_dropped_chunks: 0,
            registration: Registration {
                launch: Launch {
                    id: B.into(),
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
                in_place: false,
            },
            resume_published: None,
            opencode_title_published: None,
            commands,
            pid: None,
        },
    );
    (Arc::new(host), b)
}

impl Rig {
    /// `claude`: launch the recording agent under the name `claude` with
    /// `--session-id A`, i.e. as a fresh Claude session the wrapper recognises.
    /// `reading`: the agent reads its PTY from the start.
    async fn start(claude: bool, reading: bool) -> Rig {
        Self::start_as(conversations_core::AgentKind::Claude, claude, reading).await
    }

    /// A Claude terminal running the stateful work stub.
    async fn start_work_stub() -> Rig {
        Self::start_with(
            conversations_core::AgentKind::Claude,
            true,
            true,
            work_stub,
        )
        .await
    }

    /// [`Rig::start`] registering the terminal as `agent`.
    async fn start_as(kind: conversations_core::AgentKind, claude: bool, reading: bool) -> Rig {
        Self::start_with(kind, claude, reading, agent_body).await
    }

    async fn start_with(
        kind: conversations_core::AgentKind,
        claude: bool,
        reading: bool,
        body: fn(&Path) -> String,
    ) -> Rig {
        let central = central().await;
        let (w, _fake) = world();
        let dir = w.dir().to_path_buf();
        let agent = dir.join("agent");
        std::fs::create_dir(&agent).unwrap();
        if reading {
            std::fs::write(agent.join("go"), b"").unwrap();
        }
        let command = if claude {
            // Launched under the agent's own program name, so the wrapper
            // recognises it (a fresh Claude session, a fresh Codex launch).
            let bin = agent.join("bin");
            std::fs::create_dir(&bin).unwrap();
            let script = bin.join(match kind {
                conversations_core::AgentKind::Codex => "codex",
                conversations_core::AgentKind::OpenCode => "opencode",
                _ => "claude",
            });
            std::fs::write(&script, format!("#!/bin/sh\n{}\n", body(&agent))).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut command = vec![script.display().to_string()];
            if kind == conversations_core::AgentKind::Claude {
                command.extend(["--session-id".into(), A.into()]);
            }
            command
        } else {
            vec!["/bin/sh".into(), "-c".into(), body(&agent)]
        };
        if kind != conversations_core::AgentKind::Claude {
            // The fixture records A as a Claude session; this terminal is another agent.
            Store::open(&dir)
                .unwrap()
                .connection()
                .execute(
                    "UPDATE sessions SET agent = ?1 WHERE session_id = ?2",
                    [kind.as_str(), A],
                )
                .unwrap();
        }
        let (host, b) = host_for(&w, &central);
        let current = Arc::new(Mutex::new(host.clone()));
        let hold = Arc::new(AtomicBool::new(false));
        let (held_tx, held) = tokio::sync::mpsc::unbounded_channel();
        let to_wrapper = Arc::new(Mutex::new(None));
        let links = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::UnixListener::bind(dir.join("host.sock")).unwrap();
        {
            let (current, hold, to_wrapper, links, connections) = (
                current.clone(),
                hold.clone(),
                to_wrapper.clone(),
                links.clone(),
                connections.clone(),
            );
            tokio::spawn(async move {
                while let Ok((wrapper_side, _)) = listener.accept().await {
                    let (mut from_wrapper, mut into_wrapper) = wrapper_side.into_split();
                    let (client, server_side) = tokio::net::UnixStream::pair().unwrap();
                    let host = current.lock().unwrap().clone();
                    let serving = tokio::spawn(async move {
                        let _ = handle_client(host, server_side).await;
                    });
                    let (mut from_host, mut into_host) = client.into_split();
                    // A wrapper registers first; any other local client (the
                    // CLI's revocation notice) is passed through untouched.
                    let Ok(first) = read_message::<_, ClientMessage>(&mut from_wrapper).await
                    else {
                        continue;
                    };
                    let wrapper = matches!(first, ClientMessage::Register { .. });
                    if write_message(&mut into_host, &first).await.is_err() {
                        continue;
                    }
                    let up = tokio::spawn(async move {
                        let _ = tokio::io::copy(&mut from_wrapper, &mut into_host).await;
                    });
                    if !wrapper {
                        let down = tokio::spawn(async move {
                            let _ = tokio::io::copy(&mut from_host, &mut into_wrapper).await;
                        });
                        links.lock().unwrap().extend([serving, up, down]);
                        continue;
                    }
                    connections.fetch_add(1, Ordering::SeqCst);
                    let (down_tx, mut down_rx) =
                        tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
                    *to_wrapper.lock().unwrap() = Some(down_tx.clone());
                    let writer = tokio::spawn(async move {
                        while let Some(message) = down_rx.recv().await {
                            if write_message(&mut into_wrapper, &message).await.is_err() {
                                return;
                            }
                        }
                    });
                    let (hold, held_tx) = (hold.clone(), held_tx.clone());
                    let down = tokio::spawn(async move {
                        while let Ok(message) =
                            read_message::<_, ServerMessage>(&mut from_host).await
                        {
                            let _ = if hold.load(Ordering::SeqCst) {
                                held_tx.send(message).map_err(|_| ())
                            } else {
                                down_tx.send(message).map_err(|_| ())
                            };
                        }
                    });
                    links.lock().unwrap().extend([serving, up, writer, down]);
                }
            });
        }
        let launch = Launch {
            id: A.into(),
            agent: kind,
            cwd: dir.clone(),
            command,
        };
        let wrapper = {
            let (dir, server) = (dir.clone(), central.url.clone());
            tokio::spawn(async move {
                crate::commands::local_host::wrapper::run(&dir, launch, true, &server).await
            })
        };
        let rig = Rig {
            w,
            host,
            current,
            b,
            central,
            hold,
            held,
            to_wrapper,
            links,
            connections,
            wrapper,
            agent,
        };
        assert!(
            rig.until(4000, |r| r.host.sessions.lock().unwrap().contains_key(A)
                && r.agent.join("agent.pid").exists())
                .await,
            "the wrapper never registered with the host"
        );
        rig
    }

    async fn until(&self, ms: u64, done: impl Fn(&Rig) -> bool) -> bool {
        for _ in 0..ms / 20 {
            if done(self) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        done(self)
    }

    fn rec(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.agent.join("agent.rec")).unwrap_or_default())
            .into_owned()
    }

    async fn received(&self, text: &str) -> bool {
        self.until(5000, |r| r.rec().contains(text)).await
    }

    fn pid(&self) -> i32 {
        std::fs::read_to_string(self.agent.join("agent.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// The recording agent process and its wrapper are both still running.
    fn alive(&self, pid: i32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        let exists = unsafe { libc::kill(pid, 0) == 0 };
        exists && !self.wrapper.is_finished()
    }

    fn go(&self) {
        std::fs::write(self.agent.join("go"), b"").unwrap();
    }

    fn hold(&self, on: bool) {
        self.hold.store(on, Ordering::SeqCst);
    }

    /// The next message the host queued for A's wrapper while held.
    async fn next_held(&mut self) -> ServerMessage {
        tokio::time::timeout(Duration::from_secs(5), self.held.recv())
            .await
            .expect("the host queued nothing for the wrapper")
            .unwrap()
    }

    fn release(&self, message: ServerMessage) {
        self.to_wrapper
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(message)
            .unwrap();
    }

    /// The frame as its assigned transport hands it to the host
    /// ([`deliver_signed`]): private operations (input, replies, transfers,
    /// grants) as the private channel does once it has authenticated and
    /// decrypted them; agent messages through the relay entry point.
    async fn send(&self, frame: SignedFrame) -> (OpStatus, Option<String>) {
        deliver_signed(&self.host, frame).await
    }

    fn input_frame(&self, who: &Signer, session: &str, text: &str) -> SignedFrame {
        let args = format!(
            r#"{{"session":"{session}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
            self.w.next_seq(),
            B64.encode(text)
        );
        self.w.frame(who, "session.input", session, &args, 20)
    }

    async fn input(&self, who: &Signer, session: &str, text: &str) -> (OpStatus, Option<String>) {
        self.send(self.input_frame(who, session, text)).await
    }

    /// A host restart: a FRESH host (no memory) on the same authority database.
    /// The wrapper's socket is dropped and it reconnects to the new host.
    async fn restart(&mut self) {
        let (host, b) = host_for(&self.w, &self.central);
        *self.current.lock().unwrap() = host.clone();
        self.host = host;
        self.b = b;
        let before = self.connections.load(Ordering::SeqCst);
        for link in self.links.lock().unwrap().drain(..) {
            link.abort();
        }
        assert!(
            self.until(8000, |r| r.connections.load(Ordering::SeqCst) > before
                && r.host.sessions.lock().unwrap().contains_key(A))
                .await,
            "the wrapper did not reconnect to the restarted host"
        );
    }

    fn dir(&self) -> &Path {
        self.w.dir()
    }

    fn sql(&self, statement: &str, kid: &str) {
        Store::open(self.dir())
            .unwrap()
            .connection()
            .execute(statement, [kid])
            .unwrap();
    }
}

/// `credential revoke` without the host notification (the store call the
/// CLI makes first; nothing is interrupted).
fn revoke_credential(rig: &Rig, kid: &str) {
    Store::open(rig.dir())
        .unwrap()
        .revoke_credential(kid, now_secs())
        .unwrap();
}

/// The whole `credential revoke` path (store, then the running host's
/// interruptions), on its own thread: it runs its own runtime.
async fn cli_revoke(
    dir: &Path,
    kid: &str,
) -> (
    authz::store::Revocation,
    Option<Vec<crate::commands::local_host::protocol::Interruption>>,
) {
    let (dir, kid) = (dir.to_path_buf(), kid.to_owned());
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::commands::local_host::revoke_and_interrupt(
            &dir, &kid,
        ));
    });
    rx.await.unwrap().unwrap()
}

/// The owner signs `grant.remove` of `session.write` on A for `kid` while the
/// wrapper link is held. Returns once the removal has committed: the host's
/// interaction cancel for A is then the next held message (returned, to be
/// released by the caller), with the still-running dispatch.
async fn remove_write_on_a(
    rig: &mut Rig,
    owner: &Signer,
    kid: &str,
) -> (
    ServerMessage,
    tokio::task::JoinHandle<(OpStatus, Option<String>)>,
) {
    let frame = rig.w.frame(
        owner,
        "grant.remove",
        A,
        &format!(r#"{{"session":"{A}","recipient":"{kid}","caps":["session.write"]}}"#),
        60,
    );
    let removing = {
        let host = rig.host.clone();
        tokio::spawn(async move { deliver_signed(&host, frame).await })
    };
    let cancel = rig.next_held().await;
    assert!(
        matches!(cancel, ServerMessage::CancelInteraction { .. }),
        "{cancel:?}"
    );
    (cancel, removing)
}

fn guest_and_owner(rig: &Rig) -> (Signer, Signer) {
    let (owner, guest) = (Signer::new(), Signer::new());
    rig.w
        .enroll(
            "owner",
            &owner,
            &[
                format!("session.write:{A}"),
                format!("session.write:{B}"),
                format!("session.manage_sharing:{A}"),
            ],
        )
        .unwrap();
    rig.w
        .enroll("owner", &guest, &[format!("session.write:{A}")])
        .unwrap();
    (owner, guest)
}

/// After the revocation: the same agent process is alive, takes a later turn
/// from an unaffected credential, and another session still works.
async fn unaffected_still_work(rig: &mut Rig, owner: &Signer, pid: i32, marker: &str) {
    assert!(
        ok(&rig.input(owner, A, marker).await),
        "the owner's input was refused"
    );
    assert!(
        rig.received(marker).await,
        "the owner's later input never reached the agent"
    );
    assert_eq!(rig.pid(), pid);
    assert!(rig.alive(pid), "the agent process or its wrapper exited");
    assert!(ok(&rig.input(owner, B, "on-b").await));
    assert_eq!(inputs(Terminals::drain(&mut rig.b)), vec!["on-b"]);
}

#[test]
fn input_queued_before_a_credential_revocation_never_reaches_the_agent() {
    if !isolated(
        "write::revocation::input_queued_before_a_credential_revocation_never_reaches_the_agent",
    ) {
        return;
    }
    runtime().block_on(async {
        let mut rig = Rig::start(false, true).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "guest-ok ").await));
        assert!(rig.received("guest-ok ").await);

        // Admitted and queued for the wrapper, then the credential is revoked.
        rig.hold(true);
        assert!(ok(&rig.input(&guest, A, "QUEUED-SECRET").await));
        let queued = rig.next_held().await;
        revoke_credential(&rig, &guest.kid);
        rig.hold(false);
        rig.release(queued);
        assert!(refused(&rig.input(&guest, A, "LATE-SECRET").await));

        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
        let rec = rig.rec();
        assert!(
            !rec.contains("QUEUED-SECRET"),
            "queued input ran after revocation: {rec:?}"
        );
        assert!(!rec.contains("LATE-SECRET"));
    });
}

#[test]
fn a_reply_whose_grant_is_removed_between_body_and_enter_is_never_submitted() {
    if !isolated("write::revocation::a_reply_whose_grant_is_removed_between_body_and_enter_is_never_submitted") {
        return;
    }
    runtime().block_on(async {
        let mut rig = Rig::start(false, true).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        rig.hold(true);
        let reply = rig.w.frame(
            &guest,
            "session.reply",
            A,
            &format!(r#"{{"session":"{A}","text":"draft-text"}}"#),
            60,
        );
        let sending = {
            let host = rig.host.clone();
            tokio::spawn(async move { deliver_signed(&host, reply).await })
        };
        let body = rig.next_held().await;
        rig.release(body);
        let enter = rig.next_held().await;
        let (cancel, removing) = remove_write_on_a(&mut rig, &owner, &guest.kid).await;
        rig.hold(false);
        rig.release(enter);
        rig.release(cancel);
        let _ = sending.await.unwrap();
        assert_removed(removing.await.unwrap());
        assert!(
            rig.received("draft-text").await,
            "the body was written before the removal"
        );

        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
        let rec = rig.rec();
        assert!(
            !rec.contains('\r'),
            "the reply was submitted after its grant was removed: {rec:?}"
        );
    });
}

#[test]
fn a_delegated_key_stops_at_the_terminal_when_its_parent_is_revoked_after_admission() {
    if !isolated("write::revocation::a_delegated_key_stops_at_the_terminal_when_its_parent_is_revoked_after_admission") {
        return;
    }
    use crate::commands::local_host::authz::envelope::{key_id, DOMAIN_OP};
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    runtime().block_on(async {
        let mut rig = Rig::start(false, true).await;
        let (owner, _guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        // A passkey parent with write on A and a browser key delegated by it, as
        // the passkey ceremonies leave them.
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let browser = pair.public_key().as_ref().to_vec();
        let kid = key_id(&browser);
        let parent = key_id(b"passkey-parent-credential");
        let dlg = "00112233445566778899aabbccddeeff";
        let now = now_secs();
        {
            let store = Store::open(rig.dir()).unwrap();
            let conn = store.connection();
            conn.execute(
                "INSERT INTO credentials(cred_id, kind, alg, public_key, label, ceiling, approved_at, auth)
                 VALUES(?1, 'owner', 'es256', x'00', 'passkey', 'session.write', ?2, 'passkey')",
                rusqlite::params![parent, now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO grants(cred_id, session_id, cap, granted_at) VALUES(?1, ?2, 'session.write', ?3)",
                rusqlite::params![parent, A, now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO delegations(dlg_id, parent_cred, browser_kid, browser_pub, alg, caps, issued_at, not_after)
                 VALUES(?1, ?2, ?3, ?4, 'es256', 'session.write', ?5, ?6)",
                rusqlite::params![dlg, parent, kid, browser, now, now + 600],
            )
            .unwrap();
        }
        let host_id = rig.w.host_id.clone();
        let sign = |text: &str, seq: u64, jti: u64| {
            let args = format!(
                r#"{{"session":"{A}","ch":"{CH}","seq":{seq},"b64":"{}"}}"#,
                B64.encode(text)
            );
            let payload = format!(
                r#"{{"v":1,"typ":"op","aud":"{}","kid":"{kid}","dlg":"{dlg}","op":"session.input","sess":"{A}","jti":"{jti:032x}","iat":{now},"exp":{},"args":{args}}}"#,
                host_id,
                now + 30
            );
            let mut message = DOMAIN_OP.to_vec();
            message.extend_from_slice(payload.as_bytes());
            SignedFrame {
                payload: payload.into_bytes(),
                sig: pair.sign(&rng, &message).unwrap().as_ref().to_vec(),
                kid: kid.clone(),
            }
        };
        assert!(ok(&rig.send(sign("dlg-ok ", 1, 0xd1)).await));
        assert!(rig.received("dlg-ok ").await);
        rig.hold(true);
        assert!(ok(&rig.send(sign("DLG-SECRET", 2, 0xd2)).await));
        let queued = rig.next_held().await;
        rig.sql(
            "UPDATE credentials SET revoked_at = strftime('%s','now') WHERE cred_id = ?1",
            &parent,
        );
        rig.hold(false);
        rig.release(queued);
        assert!(refused(&rig.send(sign("DLG-LATE", 3, 0xd3)).await));

        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
        let rec = rig.rec();
        assert!(!rec.contains("DLG-SECRET"), "delegated input ran after its parent was revoked: {rec:?}");
    });
}

#[test]
fn input_queued_behind_a_busy_terminal_is_dropped_when_revoked_even_across_a_host_restart() {
    if !isolated("write::revocation::input_queued_behind_a_busy_terminal_is_dropped_when_revoked_even_across_a_host_restart") {
        return;
    }
    runtime().block_on(async {
        // Hold one admitted effect under the writer lock; acknowledge later
        // input at the wrapper queue before committing revocation.
        let mut rig = Rig::start(false, false).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        let barrier = Barrier::install(rig.dir());
        assert!(ok(&rig.input(&guest, A, "first").await));
        barrier.entered().await;
        assert!(ok(&rig.input(&guest, A, "QUEUED-SECRET").await));
        barrier.queued(2).await;
        revoke_credential(&rig, &guest.kid);
        rig.restart().await;
        rig.go();
        barrier.release();

        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
        let rec = rig.rec();
        assert!(
            rec.starts_with("first"),
            "the in-flight write completes honestly"
        );
        assert!(
            !rec.contains("QUEUED-SECRET"),
            "queued input ran after revocation and restart"
        );
    });
}
fn claude_transcript(idle: bool) -> PathBuf {
    let project = dirs::home_dir()
        .unwrap()
        .join(".claude/projects/revocation");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join(format!("{A}.jsonl"));
    let mut lines = vec![r#"{"type":"user","message":{"role":"user","content":"hi"}}"#];
    if idle {
        lines.push(r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[]}}"#);
        lines.push(r#"{"type":"system","subtype":"turn_duration"}"#);
    }
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

#[test]
fn a_queued_peer_message_from_a_revoked_sender_is_never_typed() {
    if !isolated("write::revocation::a_queued_peer_message_from_a_revoked_sender_is_never_typed") {
        return;
    }
    use crate::commands::local_host::agent_link;
    runtime().block_on(async {
        claude_transcript(true);
        let mut rig = Rig::start(true, true).await;
        let (owner, _guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        let (sender, _t) = world();
        let kid = approve_agent(rig.dir(), sender.dir(), &[A]);
        let descriptor = authz::attest::descriptor(rig.dir()).unwrap();
        agent_link::pair_peer(sender.dir(), &descriptor, A, "r").unwrap();
        // The sender's stored, signed envelope, delivered as the private channel
        // delivers it once decrypted (agent messages are channel-only).
        let p = prep(sender.dir(), "PEER-SECRET", "m1", now_secs());
        let (status, _) = deliver_signed(
            &rig.host,
            SignedFrame {
                payload: p.payload.clone(),
                sig: p.sig.clone(),
                kid: p.kid.clone(),
            },
        )
        .await;
        assert_eq!(status, conversations_core::host_relay::OpStatus::Executed);

        rig.hold(true);
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(handle_client(rig.host.clone(), server));
        write_message(
            &mut client,
            &ClientMessage::PeerInputRequest {
                id: A.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                native_session: A.into(),
                message_id: p.message_id.clone(),
            },
        )
        .await
        .unwrap();
        let queued = rig.next_held().await;
        assert!(
            matches!(queued, ServerMessage::PeerInput { .. }),
            "{queued:?}"
        );
        let (cancel, removing) = remove_write_on_a(&mut rig, &owner, &kid).await;
        rig.hold(false);
        rig.release(queued);
        rig.release(cancel);
        assert_removed(removing.await.unwrap());
        match tokio::time::timeout(
            Duration::from_secs(8),
            read_message::<_, ServerMessage>(&mut client),
        )
        .await
        .unwrap()
        .unwrap()
        {
            ServerMessage::PeerInputResult { written, .. } => {
                assert!(
                    !written,
                    "the wrapper reported typing a revoked sender's message"
                )
            }
            other => panic!("{other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!rig.rec().contains("PEER-SECRET"), "{:?}", rig.rec());
        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
    });
}

/// What the local host reported for session A after a revocation.
fn outcome_for_a(
    interrupted: &Option<Vec<crate::commands::local_host::protocol::Interruption>>,
) -> Option<crate::commands::local_host::protocol::CancelOutcome> {
    interrupted
        .as_ref()
        .expect("the running host answered")
        .iter()
        .find(|i| i.session == A)
        .and_then(|i| i.outcome)
}

fn revoke_requests_one_escape_and_keeps_the_session(kind: conversations_core::AgentKind) {
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        // A plain recorder has no native identity or turn-state integration.
        // Escape is best effort regardless of the agent's current UI state.
        let mut rig = Rig::start_as(kind, false, true).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "guest-turn ").await));
        assert!(rig.received("guest-turn ").await);

        let (revoked, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert!(!revoked.already && revoked.grants == 1, "{revoked:?}");
        assert_eq!(outcome_for_a(&interrupted), Some(CancelOutcome::Requested));
        assert!(
            interrupted.as_ref().unwrap().iter().all(|i| i.session == A),
            "an unaffected session was interrupted: {interrupted:?}"
        );
        assert!(
            rig.until(3000, |r| r.rec().contains('\u{1b}')).await,
            "no Escape reached the agent"
        );
        // Revoking again (or any later authority change) sends nothing more.
        let (again, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert!(again.already);
        assert_eq!(interrupted, Some(vec![]));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(rig.rec().matches('\u{1b}').count(), 1, "{:?}", rig.rec());
        assert!(!rig.rec().contains('\u{3}'), "a Ctrl-C reached the agent");
        assert!(refused(&rig.input(&guest, A, "AFTER-SECRET").await));

        // The SAME process takes the next, authorized turn; B still works.
        unaffected_still_work(&mut rig, &owner, pid, "owner-next-turn").await;
        assert!(!rig.rec().contains("AFTER-SECRET"));
    });
}

#[test]
fn revoking_requests_one_escape_and_keeps_the_claude_session() {
    if !isolated("write::revocation::revoking_requests_one_escape_and_keeps_the_claude_session") {
        return;
    }
    revoke_requests_one_escape_and_keeps_the_session(conversations_core::AgentKind::Claude);
}

#[test]
fn revoking_requests_one_escape_and_keeps_the_codex_session() {
    if !isolated("write::revocation::revoking_requests_one_escape_and_keeps_the_codex_session") {
        return;
    }
    revoke_requests_one_escape_and_keeps_the_session(conversations_core::AgentKind::Codex);
}

#[test]
fn revoking_requests_one_escape_and_keeps_the_opencode_session() {
    if !isolated("write::revocation::revoking_requests_one_escape_and_keeps_the_opencode_session") {
        return;
    }
    revoke_requests_one_escape_and_keeps_the_session(conversations_core::AgentKind::OpenCode);
}

#[test]
fn revoking_while_claude_is_idle_still_requests_one_escape_and_keeps_running() {
    if !isolated("write::revocation::revoking_while_claude_is_idle_still_requests_one_escape_and_keeps_running") {
        return;
    }
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        claude_transcript(true);
        let mut rig = Rig::start(true, true).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "guest-draft").await));
        assert!(rig.received("guest-draft").await);
        let (_, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert_eq!(outcome_for_a(&interrupted), Some(CancelOutcome::Requested));
        let (_, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert_eq!(interrupted, Some(vec![]));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(rig.rec().matches('\u{1b}').count(), 1, "{:?}", rig.rec());
        assert!(rig.alive(pid));
        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
    });
}

#[test]
fn an_agent_without_a_non_exiting_cancel_reports_unsupported_and_keeps_running() {
    if !isolated("write::revocation::an_agent_without_a_non_exiting_cancel_reports_unsupported_and_keeps_running") {
        return;
    }
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        let mut rig = Rig::start_as(conversations_core::AgentKind::Zsh, false, true).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "guest-cmd").await));
        assert!(rig.received("guest-cmd").await);
        let before = rig.rec();
        let (_, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert_eq!(
            outcome_for_a(&interrupted),
            Some(CancelOutcome::Unsupported)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            rig.rec(),
            before,
            "bytes were sent for an unsupported cancel"
        );
        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
    });
}

/// The removal executed; its report then says what each affected session's
/// interruption did (asserted exactly where that is the claim under test).
fn assert_removed(result: (OpStatus, Option<String>)) {
    assert!(
        matches!(&result, (OpStatus::Executed, Some(detail)) if detail.starts_with("removed; ")),
        "{result:?}"
    );
}

/// The owner signs `grant.remove` of the guest's write on A.
async fn signed_remove_on_a(rig: &Rig, owner: &Signer, kid: &str) -> (OpStatus, Option<String>) {
    let frame = rig.w.frame(
        owner,
        "grant.remove",
        A,
        &format!(r#"{{"session":"{A}","recipient":"{kid}","caps":["session.write"]}}"#),
        60,
    );
    rig.send(frame).await
}

/// A signed `grant.remove` caller learns what happened to the affected work,
/// not just "removed": here the terminal (Zsh) has no non-exiting cancel. The
/// authority is removed regardless: never rolled back to fake an interruption.
#[test]
fn a_signed_grant_remove_reports_that_the_agent_cannot_be_interrupted() {
    if !isolated(
        "write::revocation::a_signed_grant_remove_reports_that_the_agent_cannot_be_interrupted",
    ) {
        return;
    }
    runtime().block_on(async {
        let mut rig = Rig::start_as(conversations_core::AgentKind::Zsh, false, true).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "guest-cmd").await));
        assert!(rig.received("guest-cmd").await);

        let (status, detail) = signed_remove_on_a(&rig, &owner, &guest.kid).await;
        assert_eq!(status, OpStatus::Executed);
        assert_eq!(
            detail.as_deref(),
            Some(format!("removed; {A}: interrupt unsupported for this agent").as_str())
        );
        assert!(
            refused(&rig.input(&guest, A, "late").await),
            "the removal was rolled back"
        );
        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
    });
}

/// The same, while a write admitted before the removal still blocks the
/// terminal: the caller is told it may complete and nothing was interrupted.
#[test]
fn a_signed_grant_remove_reports_a_busy_terminal_as_not_interrupted() {
    if !isolated(
        "write::revocation::a_signed_grant_remove_reports_a_busy_terminal_as_not_interrupted",
    ) {
        return;
    }
    runtime().block_on(async {
        let mut rig = Rig::start(false, false).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        let barrier = Barrier::install(rig.dir());
        assert!(ok(&rig.input(&guest, A, "IN-FLIGHT").await));
        barrier.entered().await;
        barrier.queued(1).await;

        let (status, detail) = signed_remove_on_a(&rig, &owner, &guest.kid).await;
        assert_eq!(status, OpStatus::Executed);
        assert_eq!(
            detail.as_deref(),
            Some(format!(
                "removed; {A}: terminal busy: one write admitted before the revocation may still complete; not interrupted"
            ).as_str())
        );
        rig.go();
        barrier.release();
        assert!(refused(&rig.input(&guest, A, "late").await), "the removal was rolled back");
        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
    });
}

#[test]
fn revocation_with_no_running_host_is_persisted_and_reports_interruption_unavailable() {
    if !isolated("write::revocation::revocation_with_no_running_host_is_persisted_and_reports_interruption_unavailable") {
        return;
    }
    runtime().block_on(async {
        let w = world();
        let (w, _fake) = w;
        let (owner, guest) = (Signer::new(), Signer::new());
        w.enroll("owner", &owner, &[format!("session.write:{A}")])
            .unwrap();
        w.enroll(
            "owner",
            &guest,
            &[format!("session.write:{A}"), format!("session.write:{B}")],
        )
        .unwrap();
        assert!(!w.dir().join("host.sock").exists());
        let (revoked, interrupted) = cli_revoke(w.dir(), &guest.kid).await;
        assert_eq!((revoked.already, revoked.grants), (false, 2));
        assert_eq!(
            interrupted, None,
            "claimed an interruption with no host running"
        );
        // Persisted: a host that starts later refuses the revoked key, and the
        // other credential still works; records are kept, not deleted.
        let args = |seq: u64| {
            format!(
                r#"{{"session":"{A}","ch":"{CH}","seq":{seq},"b64":"{}"}}"#,
                B64.encode("x")
            )
        };
        assert!(refused(
            &w.send(w.frame(&guest, "session.input", A, &args(w.next_seq()), 20))
                .await
        ));
        assert!(ok(&w
            .send(w.frame(&owner, "session.input", A, &args(w.next_seq()), 20))
            .await));
        let kept: i64 = Store::open(w.dir())
            .unwrap()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM grants WHERE cred_id = ?1 AND revoked_at IS NOT NULL",
                [&guest.kid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 2);
    });
}

#[test]
fn grant_remove_takes_back_only_the_named_session_and_capabilities() {
    if !isolated(
        "write::revocation::grant_remove_takes_back_only_the_named_session_and_capabilities",
    ) {
        return;
    }
    runtime().block_on(async {
        let (w, _fake) = world();
        let (owner, guest, stranger) = (Signer::new(), Signer::new(), Signer::new());
        w.enroll(
            "owner",
            &owner,
            &[
                format!("session.write:{A}"),
                format!("session.manage_sharing:{A}"),
            ],
        )
        .unwrap();
        w.enroll(
            "owner",
            &guest,
            &[format!("session.write:{A}"), format!("session.write:{B}")],
        )
        .unwrap();
        let remove = |who: &Signer, session: &str, recipient: &str, caps: &str| {
            w.frame(
                who,
                "grant.remove",
                session,
                &format!(r#"{{"session":"{session}","recipient":"{recipient}","caps":{caps}}}"#),
                60,
            )
        };
        let input = |who: &Signer, session: &str| {
            let args = format!(
                r#"{{"session":"{session}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
                w.next_seq(),
                B64.encode("x")
            );
            w.frame(who, "session.input", session, &args, 20)
        };
        // No sharing authority on B; a non-grantable capability; unknown recipient.
        assert!(refused(
            &w.send(remove(&owner, B, &guest.kid, r#"["session.write"]"#))
                .await
        ));
        assert!(refused(
            &w.send(remove(
                &owner,
                A,
                &guest.kid,
                r#"["session.manage_sharing"]"#
            ))
            .await
        ));
        assert!(refused(
            &w.send(remove(&owner, A, &stranger.kid, r#"["session.write"]"#))
                .await
        ));
        // A recipient cannot use it on the owner.
        assert!(refused(
            &w.send(remove(&guest, A, &owner.kid, r#"["session.write"]"#))
                .await
        ));
        assert!(ok(&w.send(input(&guest, A)).await));

        let removed = w
            .send(remove(&owner, A, &guest.kid, r#"["session.write"]"#))
            .await;
        assert!(ok(&removed), "{removed:?}");
        assert!(refused(&w.send(input(&guest, A)).await));
        assert!(
            ok(&w.send(input(&guest, B)).await),
            "another session's grant was removed"
        );
        assert!(ok(&w.send(input(&owner, A)).await));
    });
}

/// The recipient also holds a HOST-WIDE `session.write` (it authorizes every
/// session). Removing only the session's explicit grant would leave the write
/// authority in place, so the removal is refused atomically, saying why:
/// nothing changes, it is never reported as removed, and other sessions and
/// keys carry on. Taking a host-wide grant back is a local decision
/// (`credential revoke`), not a per-session `grant.remove`.
#[test]
fn grant_remove_is_refused_while_a_host_wide_grant_still_authorizes_the_session() {
    if !isolated("write::revocation::grant_remove_is_refused_while_a_host_wide_grant_still_authorizes_the_session") {
        return;
    }
    runtime().block_on(async {
        let (w, _fake) = world();
        let (owner, guest) = (Signer::new(), Signer::new());
        w.enroll(
            "owner",
            &owner,
            &[format!("session.write:{A}"), format!("session.manage_sharing:{A}")],
        )
        .unwrap();
        w.enroll("owner", &guest, &["session.write".to_string(), format!("session.write:{A}")])
            .unwrap();
        let input = |who: &Signer, session: &str| {
            let args = format!(
                r#"{{"session":"{session}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
                w.next_seq(),
                B64.encode("x")
            );
            w.frame(who, "session.input", session, &args, 20)
        };
        let grants = || -> Vec<(String, String, Option<i64>)> {
            let store = Store::open(w.dir()).unwrap();
            let mut statement = store
                .connection()
                .prepare("SELECT session_id, cap, revoked_at FROM grants WHERE cred_id = ?1 ORDER BY session_id, cap")
                .unwrap();
            statement
                .query_map([&guest.kid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = grants();

        let removed = w
            .send(w.frame(
                &owner,
                "grant.remove",
                A,
                &format!(r#"{{"session":"{A}","recipient":"{}","caps":["session.write"]}}"#, guest.kid),
                60,
            ))
            .await;
        assert!(refused(&removed), "{removed:?}");
        let reason = removed.1.unwrap_or_default();
        assert!(reason.contains("host-wide"), "the refusal does not say why: {reason}");
        assert_eq!(grants(), before, "a refused removal changed grants");
        // Truthful either way: the guest's write authority is what it was.
        assert!(ok(&w.send(input(&guest, A)).await));
        assert!(ok(&w.send(input(&guest, B)).await));
        assert!(ok(&w.send(input(&owner, A)).await));
    });
}

/// The stateful work stub (interruption evidence). A strict model of ONE active
/// operation, in the system Perl (no agent is installed or reached):
/// - `WORK <name>` + Enter starts work: the Claude transcript shows an active
///   turn and `agent.work` gains a `tick <name>` line every 50 ms (the running
///   effect); after 60 ticks it logs `done <name>` and the turn ends.
/// - Escape while working is its non-exiting cancel: work stops, `cancel <name>`
///   is logged, the turn ends, and the process keeps reading for the next turn.
/// - Anything else it does not model (Ctrl-C, other control bytes, Escape while
///   idle, a second WORK while busy) is logged as a `VIOLATION`; it never exits.
///
/// Its events go to `agent.events`, its PID to `agent.pid`.
fn work_stub(agent: &Path) -> String {
    let transcript = claude_transcript(true);
    let script = agent.join("work.pl");
    std::fs::write(
        &script,
        r#"use strict; use warnings; use IO::Select;
my ($dir, $transcript) = @ARGV;
sub append { my ($file, $line) = @_; open(my $f, '>>', $file) or die; print $f "$line\n"; close $f; }
sub event { append("$dir/agent.events", $_[0]); }
open(my $p, '>', "$dir/agent.pid") or die; print $p "$$\n"; close $p;
system('/bin/stty', 'raw', '-echo');
$| = 1;
my ($line, $job, $ticks) = ('', undef, 0);
my $select = IO::Select->new(\*STDIN);
sub end_turn {
  append($transcript, '{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[]}}');
  append($transcript, '{"type":"system","subtype":"turn_duration"}');
  undef $job;
}
while (!-e "$dir/stop") {
  if ($select->can_read(0.05)) {
    my $n = sysread(STDIN, my $byte, 1);
    last unless $n;
    if ($byte eq "\e") {
      if (defined $job) { event("cancel $job"); end_turn(); } else { event("VIOLATION escape while idle"); }
    } elsif ($byte eq "\r") {
      if ($line =~ /^WORK (\w+)$/) {
        if (defined $job) { event("VIOLATION work while busy: $1"); }
        else { $job = $1; $ticks = 0; append($transcript, '{"type":"user","message":{"role":"user","content":"work"}}'); event("start $job"); }
      } elsif (length $line) { event("VIOLATION line: $line"); }
      $line = '';
    } elsif ($byte =~ /[\x20-\x7e]/) { $line .= $byte; }
    else { event(sprintf("VIOLATION byte 0x%02x", ord $byte)); }
  }
  if (defined $job) {
    $ticks++;
    append("$dir/agent.work", "tick $job");
    if ($ticks >= 60) { event("done $job"); end_turn(); }
  }
}
open(my $done, '>', "$dir/agent.stopped"); close $done;
"#,
    )
    .unwrap();
    format!(
        "exec /usr/bin/perl '{}' '{}' '{}'",
        script.display(),
        agent.display(),
        transcript.display()
    )
}

impl Rig {
    fn events(&self) -> String {
        std::fs::read_to_string(self.agent.join("agent.events")).unwrap_or_default()
    }

    fn ticks(&self, job: &str) -> usize {
        std::fs::read_to_string(self.agent.join("agent.work"))
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == format!("tick {job}"))
            .count()
    }

    async fn event(&self, line: &str, ms: u64) -> bool {
        self.until(ms, |r| r.events().lines().any(|l| l == line))
            .await
    }
}

/// Interruption, not just byte delivery: the guest's running operation is
/// actively producing work when the guest is revoked. The host asks for the
/// agent's own cancel (one Escape), the work stops, and a LATER authorized turn
/// runs to completion in the SAME process. Nothing ends or signals it.
#[test]
fn revoking_mid_work_cancels_the_running_operation_and_a_later_turn_completes_in_the_same_process()
{
    if !isolated("write::revocation::revoking_mid_work_cancels_the_running_operation_and_a_later_turn_completes_in_the_same_process") {
        return;
    }
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        let mut rig = Rig::start_work_stub().await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "WORK guest\r").await));
        assert!(
            rig.event("start guest", 5000).await,
            "events: {}",
            rig.events()
        );
        assert!(
            rig.until(3000, |r| r.ticks("guest") >= 3).await,
            "the operation is not running"
        );

        let (_, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert_eq!(outcome_for_a(&interrupted), Some(CancelOutcome::Requested));
        assert!(
            rig.event("cancel guest", 3000).await,
            "events: {}",
            rig.events()
        );
        let stopped = rig.ticks("guest");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            rig.ticks("guest"),
            stopped,
            "the revoked operation kept running"
        );
        assert!(!rig.events().contains("done guest"));
        assert_eq!(rig.pid(), pid);
        assert!(rig.alive(pid), "the agent exited on revocation");

        // A later authorized turn runs to completion in the same process.
        assert!(ok(&rig.input(&owner, A, "WORK owner\r").await));
        assert!(
            rig.event("start owner", 5000).await,
            "events: {}",
            rig.events()
        );
        assert!(
            rig.event("done owner", 8000).await,
            "events: {}",
            rig.events()
        );
        assert_eq!(rig.ticks("owner"), 60);
        assert_eq!(rig.pid(), pid);
        assert!(rig.alive(pid));
        assert!(
            !rig.events().contains("VIOLATION"),
            "events: {}",
            rig.events()
        );
        assert!(ok(&rig.input(&owner, B, "on-b").await));
        assert_eq!(inputs(Terminals::drain(&mut rig.b)), vec!["on-b"]);
    });
}

/// The guest's operation is running when the revocation lands; it stops, the
/// same process then runs the owner's turn to completion, and session B
/// (another terminal) never received an interaction cancel.
async fn revoked_work_stops_and_the_session_carries_on(rig: &mut Rig, owner: &Signer, pid: i32) {
    assert!(
        rig.event("cancel guest", 3000).await,
        "events: {}",
        rig.events()
    );
    let stopped = rig.ticks("guest");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        rig.ticks("guest"),
        stopped,
        "the revoked operation kept running"
    );
    assert!(!rig.events().contains("done guest"));
    assert!(ok(&rig.input(owner, A, "WORK owner\r").await));
    assert!(
        rig.event("done owner", 8000).await,
        "events: {}",
        rig.events()
    );
    assert_eq!(rig.ticks("owner"), 60);
    assert_eq!(rig.pid(), pid);
    assert!(rig.alive(pid), "the agent exited on revocation");
    assert!(
        !rig.events().contains("VIOLATION"),
        "events: {}",
        rig.events()
    );
    let on_b = Terminals::drain(&mut rig.b);
    assert!(
        !on_b
            .iter()
            .any(|m| matches!(m, ServerMessage::CancelInteraction { .. })),
        "an unrelated session was interrupted: {on_b:?}"
    );
    assert!(ok(&rig.input(owner, B, "on-b").await));
    assert_eq!(inputs(Terminals::drain(&mut rig.b)), vec!["on-b"]);
}

/// The host restarts while the guest's operation runs (the wrapper and agent
/// stay up and reconnect). Which authority started the work must survive the
/// restart: revoking the guest afterwards still interrupts it.
#[test]
fn revoking_after_a_host_restart_still_cancels_the_work_started_before_it() {
    if !isolated(
        "write::revocation::revoking_after_a_host_restart_still_cancels_the_work_started_before_it",
    ) {
        return;
    }
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        let mut rig = Rig::start_work_stub().await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "WORK guest\r").await));
        assert!(
            rig.event("start guest", 5000).await,
            "events: {}",
            rig.events()
        );
        rig.restart().await;
        assert!(
            rig.until(3000, |r| r.ticks("guest") >= 3).await,
            "the operation is not running"
        );

        let (_, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert_eq!(outcome_for_a(&interrupted), Some(CancelOutcome::Requested));
        revoked_work_stops_and_the_session_carries_on(&mut rig, &owner, pid).await;
    });
}

/// The revocation commits while no host runs (the CLI reports the
/// interruption unavailable). When the host comes back and the still-running
/// wrapper reconnects, the work the revoked key started is interrupted then.
#[test]
fn a_revocation_made_while_the_host_is_down_cancels_the_work_when_its_wrapper_reconnects() {
    if !isolated("write::revocation::a_revocation_made_while_the_host_is_down_cancels_the_work_when_its_wrapper_reconnects") {
        return;
    }
    runtime().block_on(async {
        let mut rig = Rig::start_work_stub().await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "WORK guest\r").await));
        assert!(
            rig.event("start guest", 5000).await,
            "events: {}",
            rig.events()
        );
        revoke_credential(&rig, &guest.kid);
        rig.restart().await;
        revoked_work_stops_and_the_session_carries_on(&mut rig, &owner, pid).await;
    });
}

/// Many other keys type into the session after the guest started its work.
/// However many there are, the guest's authority is still attributed: its
/// revocation interrupts the work, with no count or age after which it is lost.
#[test]
fn revoking_still_cancels_work_after_many_later_authorities_typed_into_the_session() {
    if !isolated("write::revocation::revoking_still_cancels_work_after_many_later_authorities_typed_into_the_session") {
        return;
    }
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        let mut rig = Rig::start_work_stub().await;
        let (owner, guest) = guest_and_owner(&rig);
        let others: Vec<Signer> = (0..20)
            .map(|_| {
                let other = Signer::new();
                rig.w
                    .enroll("owner", &other, &[format!("session.write:{A}")])
                    .unwrap();
                other
            })
            .collect();
        let pid = rig.pid();
        assert!(ok(&rig.input(&guest, A, "WORK guest\r").await));
        assert!(
            rig.event("start guest", 5000).await,
            "events: {}",
            rig.events()
        );
        // An empty line is nothing to the agent; each is a signed input.
        for other in &others {
            assert!(ok(&rig.input(other, A, "\r").await));
        }
        assert!(
            !rig.events().contains("done guest"),
            "the work ended before the revocation"
        );

        let (_, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert_eq!(outcome_for_a(&interrupted), Some(CancelOutcome::Requested));
        revoked_work_stops_and_the_session_carries_on(&mut rig, &owner, pid).await;
    });
}

/// Remove then re-grant the SAME key on the SAME session while its earlier
/// input is still queued: the re-grant is a new activation, so the old queued
/// input never revives; a NEW signed turn under the re-grant works.
#[test]
fn input_queued_before_a_remove_is_not_revived_by_a_regrant() {
    if !isolated("write::revocation::input_queued_before_a_remove_is_not_revived_by_a_regrant") {
        return;
    }
    runtime().block_on(async {
        let mut rig = Rig::start(false, true).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        rig.hold(true);
        assert!(ok(&rig.input(&guest, A, "OLD-QUEUED").await));
        let queued = rig.next_held().await;
        let (cancel, removing) = remove_write_on_a(&mut rig, &owner, &guest.kid).await;
        rig.release(cancel);
        assert_removed(removing.await.unwrap());
        let regrant = rig.w.frame(
            &owner,
            "grant.add",
            A,
            &format!(
                r#"{{"session":"{A}","recipient":"{}","caps":["session.write"]}}"#,
                guest.kid
            ),
            60,
        );
        let regranted = rig.send(regrant).await;
        assert!(ok(&regranted), "{regranted:?}");
        rig.hold(false);
        rig.release(queued);

        assert!(ok(&rig.input(&guest, A, "NEW-TURN").await));
        assert!(
            rig.received("NEW-TURN").await,
            "the re-granted key's new input never arrived"
        );
        assert!(
            !rig.rec().contains("OLD-QUEUED"),
            "a re-grant revived cancelled input: {:?}",
            rig.rec()
        );
        assert_eq!(rig.pid(), pid);
        assert!(rig.alive(pid));
    });
}

/// A short-lived signed input that waits in the queue past its own expiry is
/// never written, even though its grant still holds; a fresh one is.
#[test]
fn queued_input_past_its_signed_expiry_is_never_written() {
    if !isolated("write::revocation::queued_input_past_its_signed_expiry_is_never_written") {
        return;
    }
    runtime().block_on(async {
        let mut rig = Rig::start(false, true).await;
        let (_owner, guest) = guest_and_owner(&rig);
        let start = now_secs();
        rig.hold(true);
        let args = format!(
            r#"{{"session":"{A}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
            rig.w.next_seq(),
            B64.encode("STALE-INPUT")
        );
        assert!(ok(&rig
            .send(rig.w.frame(&guest, "session.input", A, &args, 1))
            .await));
        let queued = rig.next_held().await;
        while now_secs() <= start + 2 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        rig.hold(false);
        rig.release(queued);
        assert!(ok(&rig.input(&guest, A, "FRESH-INPUT").await));
        assert!(rig.received("FRESH-INPUT").await);
        assert!(
            !rig.rec().contains("STALE-INPUT"),
            "expired input was written: {:?}",
            rig.rec()
        );
    });
}

/// The dispatch linearization point. Each terminal effect is checked against
/// ONE snapshot immediately before its write, under the terminal writer lock;
/// a revocation is one commit. A write whose check preceded the commit is
/// already dispatched: it may finish after the commit (here it is blocked in a
/// final-writer barrier) and is reported, not undone. That is the ONLY work
/// that survives:
/// at most the one write in progress on the terminal. Everything queued behind
/// it re-checks after the commit and is refused. The revocation report does not
/// wait on the blocked write past its bound; it says the terminal was busy.
#[test]
fn only_the_write_in_progress_at_the_revocation_commit_completes() {
    if !isolated("write::revocation::only_the_write_in_progress_at_the_revocation_commit_completes")
    {
        return;
    }
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        let mut rig = Rig::start(false, false).await;
        let (owner, guest) = guest_and_owner(&rig);
        let pid = rig.pid();
        let barrier = Barrier::install(rig.dir());
        assert!(ok(&rig.input(&guest, A, "IN-FLIGHT").await));
        barrier.entered().await;
        assert!(ok(&rig.input(&guest, A, "AFTER-COMMIT").await));
        barrier.queued(2).await;
        let started = std::time::Instant::now();
        let (_, interrupted) = cli_revoke(rig.dir(), &guest.kid).await;
        assert_eq!(outcome_for_a(&interrupted), Some(CancelOutcome::WriterBusy));
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "the report waited on blocked IO"
        );
        rig.go();
        barrier.release();

        unaffected_still_work(&mut rig, &owner, pid, "owner-after").await;
        let rec = rig.rec();
        let body = rec.split("owner-after").next().unwrap();
        assert_eq!(body, "IN-FLIGHT", "only the admitted write may complete");
        assert!(
            !rec.contains("AFTER-COMMIT"),
            "work queued behind the commit ran"
        );
    });
}

/// An older wrapper registers without `effect_gate` (the field is absent from
/// its wire registration). It cannot enforce a revocation at the write, so the
/// host sends it no remote effect at all: no signed keystrokes, reply, staged
/// paste, peer message, file-receiver credentials or cancel. Its session stays
/// registered (nothing ends or detaches it) and status shows it needs an upgrade.
#[test]
fn an_older_wrapper_without_the_effect_gate_receives_no_remote_effect() {
    if !isolated(
        "write::revocation::an_older_wrapper_without_the_effect_gate_receives_no_remote_effect",
    ) {
        return;
    }
    use crate::commands::local_host::protocol::CancelOutcome;
    runtime().block_on(async {
        let central = central().await;
        let (w, _fake) = world();
        let (host, mut b) = host_for(&w, &central);
        let (owner, guest) = (Signer::new(), Signer::new());
        w.enroll(
            "owner",
            &owner,
            &[format!("session.write:{A}"), format!("session.write:{B}")],
        )
        .unwrap();
        w.enroll("owner", &guest, &[format!("session.write:{A}")])
            .unwrap();
        {
            let (listener, host) = (
                tokio::net::UnixListener::bind(w.dir().join("host.sock")).unwrap(),
                host.clone(),
            );
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(handle_client(host.clone(), socket));
                }
            });
        }

        // The registration exactly as a wrapper built before the gate sends it.
        let mut register = serde_json::to_value(ClientMessage::Register {
            registration: Registration {
                launch: Launch {
                    id: A.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: w.dir().to_path_buf(),
                    command: vec!["claude".into()],
                },
                workspace: None,
                surface: None,
                local_terminal: true,
                reconnecting: false,
                file_receiver: true,
                transfer_gate: true,
                effect_gate: true,
                in_place: false,
            },
        })
        .unwrap();
        fn strip(value: &mut serde_json::Value) -> bool {
            match value {
                serde_json::Value::Object(map) => {
                    map.remove("effect_gate").is_some() | map.values_mut().any(strip)
                }
                _ => false,
            }
        }
        assert!(strip(&mut register), "the registration had no effect_gate to strip");
        let bytes = serde_json::to_vec(&register).unwrap();
        let mut old = tokio::net::UnixStream::connect(w.dir().join("host.sock"))
            .await
            .unwrap();
        old.write_u32(bytes.len() as u32).await.unwrap();
        old.write_all(&bytes).await.unwrap();
        let received = Arc::new(Mutex::new(Vec::<ServerMessage>::new()));
        let reader = {
            let received = received.clone();
            tokio::spawn(async move {
                while let Ok(message) = read_message::<_, ServerMessage>(&mut old).await {
                    received.lock().unwrap().push(message);
                }
            })
        };
        for _ in 0..200 {
            if host.sessions.lock().unwrap().contains_key(A) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            host.sessions.lock().unwrap().contains_key(A),
            "the older wrapper never registered"
        );

        let send = |frame: SignedFrame| {
            let host = host.clone();
            async move { deliver_signed(&host, frame).await }
        };
        let upgrade = |result: &(OpStatus, Option<String>)| {
            !ok(result)
                && result
                    .1
                    .as_deref()
                    .is_some_and(|e| e.contains("predates revocation enforcement"))
        };
        let input = |who: &Signer, session: &str| {
            let args = format!(
                r#"{{"session":"{session}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
                w.next_seq(),
                B64.encode("REMOTE-KEYS")
            );
            w.frame(who, "session.input", session, &args, 20)
        };
        let result = send(input(&guest, A)).await;
        assert!(refused(&result) && upgrade(&result), "{result:?}");
        let result = send(input(&owner, A)).await;
        assert!(upgrade(&result), "{result:?}");
        let result = send(w.frame(
            &owner,
            "session.reply",
            A,
            &format!(r#"{{"session":"{A}","text":"REMOTE-REPLY"}}"#),
            60,
        ))
        .await;
        assert!(upgrade(&result), "{result:?}");
        let image = b"\x89PNG-old";
        let result = send(w.frame(
            &owner,
            "session.paste_image",
            A,
            &format!(
                r#"{{"session":"{A}","transfer":"01d00000000000000000000000000001","mime":"image/png","size":{},"sha256":"{}"}}"#,
                image.len(),
                sha(image)
            ),
            60,
        ))
        .await;
        assert!(upgrade(&result), "{result:?}");

        // A queued agent message from an approved peer is never typed into it.
        let (sender, _t) = world();
        approve_agent(w.dir(), sender.dir(), &[A]);
        let descriptor = authz::attest::descriptor(w.dir()).unwrap();
        crate::commands::local_host::agent_link::pair_peer(sender.dir(), &descriptor, A, "r")
            .unwrap();
        // The sender's stored, signed envelope, delivered as the private channel
        // delivers it once decrypted (agent messages are channel-only).
        let p = prep(sender.dir(), "REMOTE-PEER", "m-old", now_secs());
        let (status, _) = deliver_signed(
            &host,
            SignedFrame {
                payload: p.payload.clone(),
                sig: p.sig.clone(),
                kid: p.kid.clone(),
            },
        )
        .await;
        assert_eq!(status, conversations_core::host_relay::OpStatus::Executed);
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(handle_client(host.clone(), server));
        write_message(
            &mut client,
            &ClientMessage::PeerInputRequest {
                id: A.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                native_session: A.into(),
                message_id: p.message_id.clone(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_message::<_, ServerMessage>(&mut client).await.unwrap(),
            ServerMessage::PeerInputResult { written: false, .. }
        ));

        // Authority recorded for A before it reconnected through the older
        // wrapper: the revocation reports the upgrade, it does not send a cancel.
        let any = authz::cutover::Authority::sender(&guest.kid, A, None, None);
        let epoch = Store::open(w.dir()).unwrap().current_epoch(&any, now_secs()).unwrap();
        assert!(epoch.is_some());
        host.attribute(A, &authz::cutover::Authority { epoch, ..any })
            .unwrap();
        let (revoked, interrupted) = cli_revoke(w.dir(), &guest.kid).await;
        assert!(!revoked.already);
        assert_eq!(
            outcome_for_a(&interrupted),
            Some(CancelOutcome::UpgradeRequired)
        );

        let (mut status, server) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(handle_client(host.clone(), server));
        write_message(&mut status, &ClientMessage::Status).await.unwrap();
        let ServerMessage::Status { sessions, .. } =
            read_message::<_, ServerMessage>(&mut status).await.unwrap()
        else {
            panic!("no status")
        };
        let a = sessions
            .iter()
            .find(|s| s.registration.launch.id == A)
            .expect("the older wrapper's session is gone");
        assert!(!a.registration.effect_gate);

        // The unaffected, upgraded terminal still takes remote input.
        assert!(ok(&send(input(&owner, B)).await));
        assert_eq!(inputs(Terminals::drain(&mut b)), vec!["REMOTE-KEYS"]);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(host.sessions.lock().unwrap().contains_key(A));
        assert!(!reader.is_finished(), "the host closed the older wrapper's socket");
        let got = received.lock().unwrap();
        assert!(
            !got.iter().any(|m| matches!(
                m,
                ServerMessage::Input { .. }
                    | ServerMessage::Resize { .. }
                    | ServerMessage::Repaint
                    | ServerMessage::PeerInput { .. }
                    | ServerMessage::TransferAuthorized { .. }
                    | ServerMessage::CancelInteraction { .. }
                    | ServerMessage::End
                    | ServerMessage::Detach
            )),
            "the older wrapper was sent a remote effect: {got:?}"
        );
        reader.abort();
    });
}

/// Teardown gate: a test that fails mid-way (here: an intentional failed
/// assertion while the stub is running) leaves no recording agent behind.
#[test]
fn a_failed_assertion_leaves_no_owned_recording_agent() {
    if !isolated("write::revocation::a_failed_assertion_leaves_no_owned_recording_agent") {
        return;
    }
    let seen = Arc::new(Mutex::new(None));
    let failed = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            runtime().block_on(async {
                let rig = Rig::start(false, true).await;
                let pid = rig.pid();
                *seen.lock().unwrap() = Some((pid, rig.agent.clone()));
                assert!(
                    owned_child_running(pid),
                    "the stub is not our running child"
                );
                assert!(rig.alive(pid));
                assert_eq!(1, 2, "intentional failure while the stub runs");
            })
        })
        .join()
    };
    assert!(failed.is_err(), "the intentional assertion did not fail");
    let (pid, agent) = seen.lock().unwrap().clone().expect("the stub started");
    // The unwinding rig asked its fixture to stop and dropped its PTY; the
    // fixture ended on its own, and the test directory is gone. Observed
    // only: neither the rig nor this test signals a PID.
    assert!(
        !agent.exists(),
        "the failed test left its fixture directory"
    );
    assert!(
        !owned_child_running(pid),
        "recording stub {pid} outlived its failed test"
    );
}
