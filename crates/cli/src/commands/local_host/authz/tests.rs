//! Store and admission tests over real SQLite files in a private directory.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine};

use super::dispatch::{begin, decide, Decision};
use super::envelope::test_signing::Signer;
use super::envelope::Alg;
use super::envelope::Cap;
use super::store::{Grant, Kind, Store};
use super::*;

const SESSION: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
const JTI: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn migration_requires_source_read_and_destination_creation_authority_on_every_chunk() {
    use conversations_core::migration::{MigrationImport, MigrationManifest};
    let dir = private_dir();
    let owner = Signer::new();
    let reader = Signer::new();
    let host = enroll(dir.path(), "owner", &owner, &["session.start"]).unwrap();
    enroll(
        dir.path(),
        "owner",
        &reader,
        &[&format!("history.read:{SESSION}")],
    )
    .unwrap();
    let local = sessions::LocalSession {
        session_id: SESSION.into(),
        agent: conversations_core::AgentKind::Claude,
        cwd: "/local".into(),
        resume_id: Some(SESSION.into()),
        launcher: conversations_core::AgentLauncher::default(),
        adk_statusline: false,
    };
    Store::open(dir.path())
        .unwrap()
        .insert_session(&local, "adopted")
        .unwrap();
    let transfer = uuid::Uuid::new_v4().to_string();
    let now = store::now_secs();
    let read_args =
        serde_json::json!({"session":SESSION,"transfer":transfer,"offset":0}).to_string();
    let frame = |signer: &Signer, op: &str, scope: &str, args: &str, jti: &str| {
        signer.sign_bytes(
            signer
                .op_payload(&host, op, scope, args, jti, now, now + 60)
                .as_bytes(),
        )
    };
    assert!(matches!(
        decide(
            dir.path(),
            &frame(&reader, "migration.export", SESSION, &read_args, JTI),
            now
        )
        .unwrap(),
        Decision::Execute(_)
    ));
    assert!(matches!(
        decide(
            dir.path(),
            &frame(&owner, "migration.export", SESSION, &read_args, JTI),
            now
        )
        .unwrap(),
        Decision::Refused(_)
    ));
    let input = MigrationImport {
        transfer,
        destination: uuid::Uuid::new_v4().to_string(),
        cwd: "/destination".into(),
        manifest: MigrationManifest {
            agent: conversations_core::AgentKind::Claude,
            native_id: SESSION.into(),
            title: "Copy".into(),
            size: 3,
            sha256: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(ring::digest::digest(&ring::digest::SHA256, b"abc")),
        },
        offset: 0,
        b64: "YWJj".into(),
    };
    let args = serde_json::to_string(&input).unwrap();
    assert!(matches!(
        decide(
            dir.path(),
            &frame(
                &reader,
                "migration.import",
                "host",
                &args,
                "11111111111111111111111111111111"
            ),
            now
        )
        .unwrap(),
        Decision::Refused(_)
    ));
    // Multiple import chunks use session.start authority without becoming duplicate starts.
    for jti in [
        "22222222222222222222222222222222",
        "33333333333333333333333333333333",
    ] {
        let Decision::Execute(op) = decide(
            dir.path(),
            &frame(&owner, "migration.import", "host", &args, jti),
            now,
        )
        .unwrap() else {
            panic!("owner import denied")
        };
        begin(dir.path(), &op).unwrap();
    }
    // A grant removed after admission cannot write the next chunk.
    let Decision::Execute(op) = decide(
        dir.path(),
        &frame(
            &owner,
            "migration.import",
            "host",
            &args,
            "44444444444444444444444444444444",
        ),
        now,
    )
    .unwrap() else {
        panic!()
    };
    let db = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
    db.execute(
        "UPDATE grants SET revoked_at=?1 WHERE cred_id=?2 AND cap='session.start'",
        rusqlite::params![now, owner.kid],
    )
    .unwrap();
    assert!(begin(dir.path(), &op).is_err());
    assert!(Store::open(dir.path())
        .unwrap()
        .session(&input.destination)
        .unwrap()
        .is_none());
}

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn enroll(dir: &Path, kind: &str, signer: &Signer, grants: &[&str]) -> Result<String> {
    enroll_from_cli(
        dir,
        kind,
        "ed25519",
        &STANDARD.encode(&signer.public),
        "test key",
        &grants.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
    )
    .map(|(_, host_id)| host_id)
}

fn start(dir: &Path, signer: &Signer, jti: &str) -> super::dispatch::Decision {
    let host_id = host_id(dir).unwrap();
    let now = store::now_secs();
    let payload = signer.start_payload(&host_id, SESSION, "claude", jti, now, now + 60);
    decide(dir, &signer.sign_bytes(payload.as_bytes()), now).unwrap()
}

/// Probe SQLite's shared database lock from a separate process. Taking this
/// exclusive lock would let another SQLite process conclude it is the last
/// connection and remove WAL/SHM files still in use by the first process.
#[test]
fn database_lock_probe_subprocess() {
    use std::os::fd::AsRawFd;
    let Some(path) = std::env::var_os("CONVERSATIONS_TEST_LOCK_PROBE") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    // SAFETY: zero is a valid initial representation for this POD lock descriptor.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 0x4000_0002; // SQLite SHARED_FIRST (PENDING_BYTE + 2).
    lock.l_len = 510; // SQLite SHARED_SIZE.
                      // SAFETY: file is open and lock points to a valid descriptor for F_SETLK.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
    assert_eq!(
        result, -1,
        "another connection lost its SQLite database lock"
    );
    assert!(matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EACCES | libc::EAGAIN)
    ));
}

#[test]
fn opening_private_database_preserves_other_connections_locks() {
    let dir = private_dir();
    let writer = store::open_private(dir.path(), "history.db").unwrap();
    writer.execute_batch("CREATE TABLE test(value TEXT); BEGIN IMMEDIATE; INSERT INTO test VALUES('uncommitted');").unwrap();
    let probe = |stage: &str| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::local_host::authz::tests::database_lock_probe_subprocess",
                "--nocapture",
            ])
            .env(
                "CONVERSATIONS_TEST_LOCK_PROBE",
                dir.path().join("history.db"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{stage}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    probe("before second open");
    let reader = store::open_private(dir.path(), "history.db").unwrap();
    probe("after second open");
    drop(reader);
    probe("after second close");
    writer.execute_batch("COMMIT").unwrap();
    assert_eq!(
        writer
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn database_is_private_and_host_identity_persists() {
    let dir = private_dir();
    let first = host_id(dir.path()).unwrap();
    assert_eq!(
        host_id(dir.path()).unwrap(),
        first,
        "identity must be stable across opens"
    );
    assert_eq!(first.len(), 43);
    let signer = Signer::new();
    enroll(dir.path(), "owner", &signer, &["session.start"]).unwrap();
    for name in ["authz.db", "authz.db-wal", "authz.db-shm"] {
        let path = dir.path().join(name);
        if let Ok(meta) = std::fs::metadata(&path) {
            assert_eq!(
                meta.mode() & 0o077,
                0,
                "{name} must be owner-only, mode {:o}",
                meta.mode()
            );
        }
    }
    assert_eq!(
        std::fs::metadata(dir.path().join("authz.db"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn an_initialized_store_and_identity_do_not_need_the_database_writer_lock() {
    let dir = private_dir();
    let mut writer = Store::open(dir.path()).unwrap();
    let identity = writer.host_id().unwrap();
    let public = writer.host_public_key().unwrap();
    // Another CLI process may be changing grants. Opening the initialized
    // database and signing a reply must remain read-only while it owns WAL's
    // writer slot; authorization changes still use their own transactions.
    writer
        .connection()
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap();
    let mut reader = Store::open(dir.path()).unwrap();
    assert_eq!(reader.host_id().unwrap(), identity);
    let signature = reader.host_sign(b"host reply").unwrap();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public)
        .verify(b"host reply", &signature)
        .unwrap();
    writer.connection().execute_batch("ROLLBACK").unwrap();
}

#[test]
fn concurrent_first_opens_share_one_initialized_identity() {
    let dir = private_dir();
    let barrier = std::sync::Barrier::new(4);
    let identities = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    Store::open(dir.path()).unwrap().host_id().unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(identities.iter().all(|id| id == &identities[0]));
}

#[test]
fn v10_upgrade_adds_model_without_changing_host_identity() {
    let dir = private_dir();
    let mut previous = Store::open(dir.path()).unwrap();
    let identity = previous.host_id().unwrap();
    previous
        .connection()
        .execute_batch("ALTER TABLE sessions DROP COLUMN handoff_model; PRAGMA user_version = 10;")
        .unwrap();
    drop(previous);
    let mut upgraded = Store::open(dir.path()).unwrap();
    assert_eq!(upgraded.host_id().unwrap(), identity);
    let count: i64 = upgraded
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'handoff_model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn legacy_schemas_preserve_identity_and_add_restore_intent() {
    // v9 was the private-channel candidate before canonical restore intent.
    for version in [7, 9] {
        let dir = private_dir();
        let mut previous = Store::open(dir.path()).unwrap();
        let identity = previous.host_id().unwrap();
        previous
            .insert_session(
                &sessions::LocalSession {
                    session_id: SESSION.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/work".into(),
                    resume_id: Some(SESSION.into()),
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        // Both versions have canonical records but no restore-intent tables.
        previous
            .connection()
            .execute_batch("DROP TABLE local_restore; DROP TABLE local_restore_imports;")
            .unwrap();
        previous
            .connection()
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(previous);

        let mut upgraded = Store::open(dir.path()).unwrap();
        assert_eq!(upgraded.host_id().unwrap(), identity);
        upgraded
            .connection()
            .execute(
                "INSERT INTO local_restore(session_id, server) VALUES(?1, 'https://example.test')",
                [SESSION],
            )
            .unwrap();
        upgraded
            .connection()
            .execute(
                "INSERT INTO local_restore_imports(server) VALUES('https://example.test')",
                [],
            )
            .unwrap();
        drop(upgraded);
        let reopened = Store::open(dir.path()).unwrap();
        let cwd: String = reopened
            .connection()
            .query_row(
                "SELECT s.cwd FROM local_restore r JOIN sessions s USING(session_id)
                 JOIN local_restore_imports i USING(server) WHERE r.session_id = ?1",
                [SESSION],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cwd, "/work");
    }
}

#[test]
fn refuses_a_group_accessible_runtime_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(Store::open(dir.path()).is_err());
}

#[test]
fn enrollment_rules_are_enforced_locally() {
    let dir = private_dir();
    let agent = Signer::new();
    // an agent key may never be approved with a session-creating capability
    assert!(enroll(dir.path(), "agent", &agent, &["session.start"]).is_err());
    assert!(enroll(
        dir.path(),
        "agent",
        &agent,
        &[
            "session.write:3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
            "session.reopen"
        ]
    )
    .is_err());
    // closed capability set and valid key material
    assert!(enroll(dir.path(), "owner", &agent, &["session.erase"]).is_err());
    assert!(enroll(
        dir.path(),
        "owner",
        &agent,
        &["session.start:3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11"]
    )
    .is_err());
    assert!(enroll(dir.path(), "root", &agent, &["session.start"]).is_err());
    let mut store = Store::open(dir.path()).unwrap();
    assert!(store
        .enroll(
            Kind::Owner,
            Alg::Ed25519,
            &[1, 2, 3],
            "x",
            &[Grant::parse("session.start").unwrap()]
        )
        .is_err());
    drop(store);
    // a valid enrollment succeeds once; re-enrolling the same key is refused
    enroll(
        dir.path(),
        "agent",
        &agent,
        &["session.write:3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11"],
    )
    .unwrap();
    assert!(enroll(dir.path(), "agent", &agent, &["history.read"]).is_err());
}

#[test]
fn only_an_enrolled_owner_with_a_start_grant_is_admitted() {
    let dir = private_dir();
    let owner = Signer::new();
    let agent = Signer::new();
    let reader = Signer::new();
    let stranger = Signer::new();
    enroll(dir.path(), "owner", &owner, &["session.start"]).unwrap();
    enroll(
        dir.path(),
        "agent",
        &agent,
        &["session.write:3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11"],
    )
    .unwrap();
    enroll(dir.path(), "owner", &reader, &["history.read"]).unwrap();

    assert!(matches!(
        start(dir.path(), &owner, JTI),
        Decision::Execute(_)
    ));
    for (who, name) in [
        (&agent, "agent"),
        (&reader, "owner without start grant"),
        (&stranger, "unenrolled"),
    ] {
        let decision = start(dir.path(), who, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(
            matches!(decision, Decision::Refused(_)),
            "{name}: {decision:?}"
        );
    }

    // Even a grant row injected straight into the database (which the product
    // never does) cannot give an agent key session creation.
    let store = Store::open(dir.path()).unwrap();
    store
        .connection()
        .execute(
            "INSERT OR REPLACE INTO grants(cred_id, session_id, cap, granted_at) VALUES(?1, '*', 'session.start', 1)",
            [&agent.kid],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "UPDATE credentials SET ceiling = ceiling || ',session.start' WHERE cred_id = ?1",
            [&agent.kid],
        )
        .unwrap();
    drop(store);
    match start(dir.path(), &agent, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb") {
        Decision::Refused(reason) => assert!(reason.contains("cannot create sessions"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn journal_survives_restart_and_crash_recovery_never_replays() {
    let dir = private_dir();
    let owner = Signer::new();
    enroll(dir.path(), "owner", &owner, &["session.start"]).unwrap();

    // Keep the original signed bytes for replay: rebuilding the envelope after
    // a clock tick changes iat/exp and is correctly rejected as a payload mismatch.
    let host = host_id(dir.path()).unwrap();
    let now = store::now_secs();
    let first_frame = owner.sign_bytes(
        owner
            .start_payload(&host, SESSION, "claude", JTI, now, now + 60)
            .as_bytes(),
    );
    // 1. admitted, effect never started (crash right after admission)
    let Decision::Execute(first) = decide(dir.path(), &first_frame, now).unwrap() else {
        panic!()
    };
    // 2. dispatching (crash somewhere around the effect)
    let second_jti = "ffffffffffffffffffffffffffffffff";
    let other_session = "5e1aeb93-5aba-419e-8856-a7c721406279";
    let host = host_id(dir.path()).unwrap();
    let now = store::now_secs();
    let payload = owner.start_payload(&host, other_session, "claude", second_jti, now, now + 60);
    let Decision::Execute(second) =
        decide(dir.path(), &owner.sign_bytes(payload.as_bytes()), now).unwrap()
    else {
        panic!()
    };
    begin(dir.path(), &second).unwrap();

    // "restart": every Store is dropped and reopened; recovery runs once.
    assert_eq!(recover(dir.path()).unwrap(), 2);
    match decide(dir.path(), &first_frame, now + 1).unwrap() {
        Decision::Replay { state, result } => {
            assert_eq!(state, "failed");
            assert_eq!(result.as_deref(), Some("not_started_after_restart"));
        }
        other => panic!("{other:?}"),
    }
    match decide(dir.path(), &owner.sign_bytes(payload.as_bytes()), now).unwrap() {
        Decision::Replay { state, .. } => assert_eq!(state, "uncertain"),
        other => panic!("{other:?}"),
    }
    assert_ne!(first.op.session_id(), second.op.session_id());

    // a second start for an already-started session is refused outright
    let again = owner.start_payload(
        &host,
        other_session,
        "claude",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        now,
        now + 60,
    );
    assert!(matches!(
        decide(dir.path(), &owner.sign_bytes(again.as_bytes()), now).unwrap(),
        Decision::Refused(reason) if reason.contains("already started")
    ));
    // a start that provably never began (failed) may be re-signed
    assert!(matches!(
        start(dir.path(), &owner, "dddddddddddddddddddddddddddddddd"),
        Decision::Execute(_)
    ));

    // same operation id with different content is a protocol violation
    let host_id = host_id(dir.path()).unwrap();
    let now = store::now_secs();
    let other = owner.start_payload(&host_id, SESSION, "codex", JTI, now, now + 60);
    assert!(matches!(
        decide(dir.path(), &owner.sign_bytes(other.as_bytes()), now).unwrap(),
        Decision::Refused(reason) if reason.contains("reused")
    ));
}

#[test]
fn extreme_timestamps_are_refused_not_panicked() {
    let dir = private_dir();
    let owner = Signer::new();
    enroll(dir.path(), "owner", &owner, &["session.start"]).unwrap();
    let host_id = host_id(dir.path()).unwrap();
    for (iat, exp) in [
        (i64::MIN, i64::MAX),
        (i64::MAX, i64::MIN),
        (i64::MAX, i64::MAX),
        (0, 0),
        (-1, 1),
    ] {
        let payload = owner.start_payload(&host_id, SESSION, "claude", JTI, iat, exp);
        let decision = decide(
            dir.path(),
            &owner.sign_bytes(payload.as_bytes()),
            store::now_secs(),
        )
        .unwrap();
        assert!(
            matches!(decision, Decision::Refused(_)),
            "{iat} {exp}: {decision:?}"
        );
    }
}

#[test]
fn a_v6_inbox_upgrades_in_place_and_keeps_its_ids_while_new_signers_get_their_own() {
    let dir = private_dir();
    let (alice, bob) = (Signer::new(), Signer::new());
    let source = "5e1aeb93-5aba-419e-8856-a7c721406279";
    {
        // The v6 layout: inbox keyed by the bare operation id, sends without a
        // check time.
        let conn = store::open_private(dir.path(), "authz.db").unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE agent_inbox(
               message_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, source_session TEXT NOT NULL,
               kid TEXT NOT NULL, text TEXT NOT NULL, reply_to TEXT, signed_exp INTEGER NOT NULL,
               payload_sha256 BLOB NOT NULL, admitted_at INTEGER NOT NULL,
               state TEXT NOT NULL DEFAULT 'queued', state_at INTEGER NOT NULL);
             CREATE TABLE agent_sends(
               source_session TEXT NOT NULL, target_session TEXT NOT NULL, idem TEXT NOT NULL,
               request_sha256 BLOB NOT NULL, attempt INTEGER NOT NULL, message_id TEXT NOT NULL,
               payload BLOB NOT NULL, sig BLOB NOT NULL, kid TEXT NOT NULL, host_id TEXT NOT NULL,
               exp INTEGER NOT NULL, state TEXT NOT NULL, detail TEXT,
               PRIMARY KEY(source_session, target_session, idem));
             INSERT INTO agent_inbox VALUES('{JTI}', '{SESSION}', '{source}', '{}', 'legacy', NULL, 1, x'00', {now}, 'received', {now});
             INSERT INTO agent_sends VALUES('{SESSION}', '{source}', 'k', x'00', 1, '{JTI}', x'00', x'00', 'kid', 'host', 1, 'queued', NULL);
             PRAGMA user_version = 6;",
            alice.kid,
            now = store::now_secs()
        ))
        .unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    // Nothing lost: the old row keeps the id its agent was shown, and its signer
    // can still ask about it by operation id.
    assert_eq!(
        store.inbox_received(SESSION, JTI).unwrap(),
        Some(("received".into(), source.into()))
    );
    assert_eq!(
        store.inbox_state(&alice.kid, SESSION, JTI).unwrap(),
        "received"
    );
    assert_eq!(store.inbox_state(&bob.kid, SESSION, JTI).unwrap(), "absent");
    let sent = store.send_get(SESSION, source, "k").unwrap().unwrap();
    assert_eq!(
        (
            sent.message_id.as_str(),
            sent.state.as_str(),
            sent.checked_at
        ),
        (JTI, "queued", None)
    );
    drop(store);

    // Another signer reusing that operation id is a different message.
    enroll(
        dir.path(),
        "agent",
        &bob,
        &[&format!("session.write:{SESSION}")],
    )
    .unwrap();
    Store::open(dir.path())
        .unwrap()
        .insert_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: "/work".into(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    let host = host_id(dir.path()).unwrap();
    let now = store::now_secs();
    let args = format!(
        r#"{{"session":"{SESSION}","source":"{source}","source_host":"{}","text":"from bob","reply_to":null}}"#,
        bob.kid
    );
    let payload = bob.op_payload(&host, "agent.message", SESSION, &args, JTI, now, now + 60);
    let Decision::Execute(op) =
        decide(dir.path(), &bob.sign_bytes(payload.as_bytes()), now).unwrap()
    else {
        panic!("bob's message was not admitted")
    };
    let mut store = Store::open(dir.path()).unwrap();
    let id = store
        .inbox_admit(&op, source, &bob.kid, "from bob", None, now)
        .unwrap();
    assert_ne!(id, JTI);
    assert_eq!(store.inbox_state(&bob.kid, SESSION, JTI).unwrap(), "queued");
    assert_eq!(
        store.inbox_state(&alice.kid, SESSION, JTI).unwrap(),
        "received"
    );
    let texts: Vec<String> = store
        .inbox_deliverable(SESSION, now, 10)
        .unwrap()
        .into_iter()
        .map(|row| row.text)
        .collect();
    assert!(texts.contains(&"from bob".to_owned()), "{texts:?}");
}

/// A database written by the v7 host (signer-scoped inbox, no private channel)
/// upgrades in place: its credentials, grants and journal stay, it gains the
/// challenge table, and its version moves past 7 so a v7 host refuses it
/// instead of opening a schema it does not know. A newer database is refused
/// and left untouched.
#[test]
fn a_v7_database_upgrades_without_loss_and_a_newer_one_is_refused_untouched() {
    let dir = private_dir();
    let signer = Signer::new();
    enroll(dir.path(), "owner", &signer, &["session.start"]).unwrap();
    let identity = host_id(dir.path()).unwrap();
    let issued_at = store::now_secs();
    let payload =
        signer.start_payload(&identity, SESSION, "claude", JTI, issued_at, issued_at + 60);
    let signed = signer.sign_bytes(payload.as_bytes());
    assert!(matches!(
        decide(dir.path(), &signed, issued_at).unwrap(),
        Decision::Execute(_)
    ));
    {
        let conn = store::open_private(dir.path(), "authz.db").unwrap();
        conn.execute_batch(
            "DROP TABLE peer_challenges;
             DROP TABLE session_interactions;
             DROP TRIGGER grant_activated;
             DROP TRIGGER grant_reactivated;
             DROP TABLE grant_epochs;
             ALTER TABLE grants DROP COLUMN epoch;
             ALTER TABLE session_pr_targets DROP COLUMN kid;
             ALTER TABLE session_pr_targets DROP COLUMN dlg;
             ALTER TABLE session_pr_targets DROP COLUMN cap;
             ALTER TABLE session_pr_targets DROP COLUMN epoch;
             PRAGMA user_version = 7;",
        )
        .unwrap();
    }
    let mut store = Store::open(dir.path()).unwrap();
    let now = store::now_secs();
    assert!(store.issue_peer_challenge(now).unwrap().is_some());
    drop(store);
    assert_eq!(host_id(dir.path()).unwrap(), identity);
    let conn = store::open_private(dir.path(), "authz.db").unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert!(version > 7, "a v7 host would open this schema: {version}");
    let kept: (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM credentials), (SELECT COUNT(*) FROM grants), (SELECT COUNT(*) FROM op_journal)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(kept, (1, 1, 1));
    // Replay the exact signed request, even if migration crossed a clock second.
    drop(conn);
    assert!(matches!(
        decide(dir.path(), &signed, store::now_secs()).unwrap(),
        Decision::Replay { .. }
    ));

    let conn = store::open_private(dir.path(), "authz.db").unwrap();
    conn.pragma_update(None, "user_version", version + 1)
        .unwrap();
    drop(conn);
    let refused = Store::open(dir.path())
        .err()
        .expect("a newer schema opened");
    assert!(refused.to_string().contains("newer host"), "{refused:#}");
    let conn = store::open_private(dir.path(), "authz.db").unwrap();
    let after: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(after, version + 1, "the refusal rewrote the version");
    let credentials: i64 = conn
        .query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0))
        .unwrap();
    assert_eq!(credentials, 1);
}

/// The two independently developed v8 schemas must both upgrade without
/// resurrecting consumed work or losing grant cutover history. Simulate their
/// respective absent additions, not merely a smaller user_version number.
#[test]
fn both_v8_schema_branches_preserve_replay_and_authority_on_upgrade() {
    for peer_branch in [false, true] {
        let dir = private_dir();
        let signer = Signer::new();
        enroll(dir.path(), "owner", &signer, &["session.start"]).unwrap();
        let identity = host_id(dir.path()).unwrap();
        let now = store::now_secs();
        let frame = signer.sign_bytes(
            signer
                .start_payload(&identity, SESSION, "claude", JTI, now, now + 60)
                .as_bytes(),
        );
        assert!(matches!(
            decide(dir.path(), &frame, now).unwrap(),
            Decision::Execute(_)
        ));
        let conn = store::open_private(dir.path(), "authz.db").unwrap();
        if peer_branch {
            conn.execute_batch(
                "DROP TRIGGER grant_activated;
                 DROP TRIGGER grant_reactivated;
                 DROP TABLE grant_epochs;
                 ALTER TABLE grants DROP COLUMN epoch;
                 DROP TABLE session_interactions;
                 ALTER TABLE session_pr_targets DROP COLUMN kid;
                 ALTER TABLE session_pr_targets DROP COLUMN dlg;
                 ALTER TABLE session_pr_targets DROP COLUMN cap;
                 ALTER TABLE session_pr_targets DROP COLUMN epoch;",
            )
            .unwrap();
        } else {
            conn.execute_batch("DROP TABLE peer_challenges;").unwrap();
        }
        conn.pragma_update(None, "user_version", 8).unwrap();
        let grants_before: Vec<(String, String, String, i64)> = conn
            .prepare("SELECT cred_id, session_id, cap, granted_at FROM grants ORDER BY cred_id, session_id, cap")
            .unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap().collect::<rusqlite::Result<_>>().unwrap();
        drop(conn);

        let mut upgraded = Store::open(dir.path()).unwrap();
        assert!(upgraded
            .issue_peer_challenge(store::now_secs())
            .unwrap()
            .is_some());
        drop(upgraded);
        assert_eq!(host_id(dir.path()).unwrap(), identity);
        assert!(matches!(
            decide(dir.path(), &frame, now + 1).unwrap(),
            Decision::Replay { .. }
        ));
        let conn = store::open_private(dir.path(), "authz.db").unwrap();
        let grants_after: Vec<(String, String, String, i64)> = conn
            .prepare("SELECT cred_id, session_id, cap, granted_at FROM grants ORDER BY cred_id, session_id, cap")
            .unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(grants_after, grants_before);
        // No timestamp advance: reactivation must still invalidate queued work.
        let before: i64 = conn
            .query_row("SELECT COALESCE(epoch, 0) FROM grants", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(
            "UPDATE grants SET revoked_at = granted_at; UPDATE grants SET revoked_at = NULL;",
        )
        .unwrap();
        let after: i64 = conn
            .query_row("SELECT epoch FROM grants", [], |r| r.get(0))
            .unwrap();
        assert!(
            after > before,
            "v8 peer branch={peer_branch}: authority epoch reused"
        );
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            super::store::SCHEMA_VERSION
        );
    }
}

#[test]
fn v14_host_upgrade_keeps_identity_and_makes_key_offers_readable() {
    let dir = private_dir();
    let identity = host_id(dir.path()).unwrap();
    let conn = store::open_private(dir.path(), "authz.db").unwrap();
    conn.execute_batch(
        "ALTER TABLE pending_credentials DROP COLUMN vouch_host_id;
        ALTER TABLE pending_credentials DROP COLUMN vouch_host_key;
        ALTER TABLE credentials DROP COLUMN vouch_host_id;
        ALTER TABLE credentials DROP COLUMN vouch_host_key;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 14).unwrap();
    drop(conn);
    let upgraded = Store::open(dir.path()).unwrap();
    assert!(
        matches!(super::passkey::pending(&upgraded).unwrap(), super::passkey::PasskeyResponse::Pending { pending } if pending.is_empty())
    );
    assert_eq!(host_id(dir.path()).unwrap(), identity);
}

// ---- private channel: offers, challenges and private reads ----

fn offer_for(signer: &Signer, challenge: &str, dlg: Option<&str>) -> super::envelope::PeerOffer {
    super::envelope::PeerOffer {
        kid: signer.kid.clone(),
        dlg: dlg.map(str::to_owned),
        payload_sha256: vec![7; 32],
        challenge: challenge.into(),
        sdp_sha256: "A".repeat(43),
        fingerprint: format!("sha-256 {}", ["AB"; 32].join(":")),
    }
}

#[test]
fn a_peer_challenge_is_used_once_expires_and_does_not_survive_a_host_restart() {
    let dir = private_dir();
    let signer = Signer::new();
    enroll(dir.path(), "owner", &signer, &["session.start"]).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let now = store::now_secs();

    let (first, exp) = store.issue_peer_challenge(now).unwrap().unwrap();
    assert_eq!(exp, now + store::PEER_CHALLENGE_TTL_SECS);
    assert_eq!(
        store
            .admit_peer_offer(&offer_for(&signer, &first, None), now)
            .unwrap(),
        None
    );
    assert!(
        store
            .admit_peer_offer(&offer_for(&signer, &first, None), now)
            .unwrap()
            .is_some(),
        "reused"
    );

    let (late, exp) = store.issue_peer_challenge(now).unwrap().unwrap();
    assert!(
        store
            .admit_peer_offer(&offer_for(&signer, &late, None), exp + 1)
            .unwrap()
            .is_some(),
        "expired"
    );
    assert!(
        store
            .admit_peer_offer(&offer_for(&signer, &"0".repeat(32), None), now)
            .unwrap()
            .is_some(),
        "unknown"
    );

    let (before_restart, _) = store.issue_peer_challenge(now).unwrap().unwrap();
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    store.expire_peer_challenges(now).unwrap();
    assert!(
        store
            .admit_peer_offer(&offer_for(&signer, &before_restart, None), now)
            .unwrap()
            .is_some(),
        "survived a restart"
    );
}

#[test]
fn open_peer_challenges_are_bounded() {
    let dir = private_dir();
    let mut store = Store::open(dir.path()).unwrap();
    let now = store::now_secs();
    for _ in 0..16 {
        assert!(store.issue_peer_challenge(now).unwrap().is_some());
    }
    assert!(store.issue_peer_challenge(now).unwrap().is_none());
    // Expired ones no longer count.
    assert!(store.issue_peer_challenge(now + 61).unwrap().is_some());
}

/// A delegated offer needs a live, unrevoked delegation for this key whose parent
/// is an unrevoked passkey. The challenge is spent even when the answer is no.
#[test]
fn a_delegated_offer_needs_a_live_delegation_from_an_unrevoked_passkey() {
    let dir = private_dir();
    let browser = Signer::new();
    let parent = Signer::new();
    enroll(dir.path(), "owner", &parent, &["session.start"]).unwrap();
    let db = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
    db.execute(
        "UPDATE credentials SET auth = 'passkey' WHERE cred_id = ?1",
        [&parent.kid],
    )
    .unwrap();
    let now = store::now_secs();
    let dlg = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    db.execute(
        "INSERT INTO delegations(dlg_id, parent_cred, browser_kid, browser_pub, alg, caps, issued_at, not_after)
         VALUES(?1, ?2, ?3, x'00', 'es256', 'history.read', ?4, ?5)",
        rusqlite::params![dlg, parent.kid, browser.kid, now, now + 100],
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut attempt = |at: i64| {
        let (challenge, _) = store.issue_peer_challenge(at).unwrap().unwrap();
        let first = store
            .admit_peer_offer(&offer_for(&browser, &challenge, Some(dlg)), at)
            .unwrap();
        let again = store
            .admit_peer_offer(&offer_for(&browser, &challenge, Some(dlg)), at)
            .unwrap();
        assert!(
            again.is_some(),
            "a refused offer must still spend its challenge"
        );
        first
    };
    assert_eq!(attempt(now), None);
    assert!(attempt(now + 101).is_some(), "expired delegation");
    db.execute(
        "UPDATE credentials SET revoked_at = 1 WHERE cred_id = ?1",
        [&parent.kid],
    )
    .unwrap();
    assert!(attempt(now).is_some(), "revoked parent");
    db.execute(
        "UPDATE credentials SET revoked_at = NULL, auth = 'raw_key' WHERE cred_id = ?1",
        [&parent.kid],
    )
    .unwrap();
    assert!(attempt(now).is_some(), "raw-key parent");
    db.execute(
        "UPDATE credentials SET auth = 'passkey' WHERE cred_id = ?1",
        [&parent.kid],
    )
    .unwrap();
    db.execute(
        "UPDATE delegations SET revoked_at = 1 WHERE dlg_id = ?1",
        [dlg],
    )
    .unwrap();
    assert!(attempt(now).is_some(), "revoked delegation");
}

#[test]
fn a_peer_offer_is_verified_strictly_and_is_never_an_operation() {
    let dir = private_dir();
    let signer = Signer::new();
    enroll(dir.path(), "owner", &signer, &["session.start"]).unwrap();
    let host = host_id(dir.path()).unwrap();
    let now = store::now_secs();
    let fp = format!("sha-256 {}", ["AB"; 32].join(":"));
    let args = |extra: &str| {
        format!(
            r#"{{"challenge":"{JTI}","sdp_sha256":"{}","fingerprint":"{fp}"{extra}}}"#,
            "A".repeat(43)
        )
    };
    let verify = |op: &str, sess: &str, args: &str| {
        let frame = signer.sign_bytes(
            signer
                .op_payload(&host, op, sess, args, JTI, now, now + 60)
                .as_bytes(),
        );
        super::envelope::verify_peer_offer(&frame, Alg::Ed25519, &signer.public, &host, now, None)
    };
    let offer = verify("peer.offer", "host", &args("")).unwrap();
    assert_eq!(
        (offer.challenge.as_str(), offer.fingerprint.as_str()),
        (JTI, fp.as_str())
    );
    assert!(
        verify("peer.offer", SESSION, &args("")).is_err(),
        "session scoped"
    );
    assert!(
        verify("peer.offer", "host", &args(r#","x":1"#)).is_err(),
        "unknown field"
    );
    assert!(
        verify("peer.offer", "host", &args("").replace("AB:", "ab:")).is_err(),
        "fingerprint"
    );
    assert!(
        verify("session.end", "host", &args("")).is_err(),
        "another operation"
    );
    // And the operation verifier never accepts an offer.
    let frame = signer.sign_bytes(
        signer
            .op_payload(&host, "peer.offer", "host", &args(""), JTI, now, now + 60)
            .as_bytes(),
    );
    assert_eq!(
        super::envelope::verify(&frame, Alg::Ed25519, &signer.public, &host, now),
        Err(super::envelope::Reject::UnknownOperation)
    );
}

#[test]
fn private_reads_need_the_history_grant_for_that_session_and_strict_bounds() {
    let dir = private_dir();
    let reader = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &reader,
        &[&format!("history.read:{SESSION}")],
    )
    .unwrap();
    let writer = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &writer,
        &[&format!("session.write:{SESSION}")],
    )
    .unwrap();
    Store::open(dir.path())
        .unwrap()
        .insert_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: "/w".into(),
                resume_id: None,
                launcher: Default::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    let host = host_id(dir.path()).unwrap();
    let now = store::now_secs();
    let mut n = 0u32;
    let mut decide_as = |signer: &Signer, op: &str, args: &str| {
        n += 1;
        let jti = format!("{n:032x}");
        let payload = signer.op_payload(&host, op, SESSION, args, &jti, now, now + 60);
        decide(dir.path(), &signer.sign_bytes(payload.as_bytes()), now).unwrap()
    };
    let get = format!(r#"{{"session":"{SESSION}","after":0,"limit":32}}"#);
    let search = format!(r#"{{"session":"{SESSION}","query":"vault","limit":20}}"#);
    assert!(matches!(
        decide_as(&reader, "history.get", &get),
        Decision::Execute(_)
    ));
    assert!(matches!(
        decide_as(&reader, "history.search", &search),
        Decision::Execute(_)
    ));
    assert!(matches!(
        decide_as(&writer, "history.get", &get),
        Decision::Refused(_)
    ));
    for bad in [
        format!(r#"{{"session":"{SESSION}","after":0,"limit":33}}"#),
        format!(r#"{{"session":"{SESSION}","after":0,"limit":0}}"#),
        format!(r#"{{"session":"{SESSION}","after":9223372036854775808,"limit":1}}"#),
        format!(r#"{{"session":"{SESSION}","after":-1,"limit":1}}"#),
        format!(r#"{{"session":"{SESSION}","after":0}}"#),
    ] {
        assert!(
            matches!(
                decide_as(&reader, "history.get", &bad),
                Decision::Refused(_)
            ),
            "{bad}"
        );
    }
    for bad in [
        format!(
            r#"{{"session":"{SESSION}","query":"{}","limit":1}}"#,
            "q".repeat(257)
        ),
        format!(r#"{{"session":"{SESSION}","query":"  ","limit":1}}"#),
        format!(r#"{{"session":"{SESSION}","query":"x","limit":21}}"#),
    ] {
        assert!(
            matches!(
                decide_as(&reader, "history.search", &bad),
                Decision::Refused(_)
            ),
            "{bad}"
        );
    }
}

#[test]
fn a_native_session_maps_to_one_canonical_id_and_its_binding_wins() {
    use crate::commands::local_host::history::{History, Source};
    use conversations_core::{AgentKind, AgentLauncher};
    let dir = private_dir();
    ensure_dir(dir.path()).unwrap();
    let native = "ses_2f9a6c1d8e7b";

    // Not a session id and nothing recorded: the derived id, stable, a valid
    // session id, and specific to the agent.
    let derived = canonical_for_native(dir.path(), AgentKind::OpenCode, native).unwrap();
    assert_eq!(derived, derived_session_id(AgentKind::OpenCode, native));
    assert_eq!(
        canonical_for_native(dir.path(), AgentKind::OpenCode, native).unwrap(),
        derived
    );
    assert!(envelope::valid_session_id(&derived));
    assert_ne!(derived, derived_session_id(AgentKind::Claude, native));
    // A native id that is a session id stays itself.
    assert_eq!(
        canonical_for_native(dir.path(), AgentKind::Claude, SESSION).unwrap(),
        SESSION
    );

    // History already stored under a session: that session.
    let stored = "7c2e9f1a-3b4d-4e5f-8a6b-1c2d3e4f5a6b";
    let message = conversations_core::parse_line(&serde_json::json!({
        "type": "user", "uuid": "m1", "sessionId": native,
        "timestamp": "2026-11-02T10:00:00.000Z",
        "message": {"role": "user", "content": "x"}
    }))
    .unwrap();
    History::open(dir.path())
        .unwrap()
        .ingest(
            stored,
            &Source {
                agent: AgentKind::OpenCode,
                native_id: native.into(),
                origin: "db".into(),
            },
            &[message],
        )
        .unwrap();
    assert_eq!(
        canonical_for_native(dir.path(), AgentKind::OpenCode, native).unwrap(),
        stored
    );

    // A canonical record bound to the native session wins over both.
    let bound = "1a2b3c4d-5e6f-4a1b-9c2d-3e4f5a6b7c8d";
    Store::open(dir.path())
        .unwrap()
        .insert_session(
            &sessions::LocalSession {
                session_id: bound.into(),
                agent: AgentKind::OpenCode,
                cwd: "/work".into(),
                resume_id: Some(native.into()),
                launcher: AgentLauncher::default(),
                adk_statusline: false,
            },
            "adopted",
        )
        .unwrap();
    assert_eq!(
        canonical_for_native(dir.path(), AgentKind::OpenCode, native).unwrap(),
        bound
    );
}

/// An explicitly selected OpenCode session gets a canonical record the signed
/// read path can address: listed, read and searched under its canonical id by
/// a key granted read access, the same record every time, and a conflicting
/// record is refused rather than retargeted.
#[test]
fn an_adopted_native_session_is_listed_read_and_searched_through_signed_grants() {
    use crate::commands::local_host::history::{History, Source};
    use conversations_core::AgentKind;
    let dir = private_dir();
    let native = "ses_5b1e0c7a9d2f";
    let reader = Signer::new();
    enroll(dir.path(), "owner", &reader, &["history.read"]).unwrap();
    let host = host_id(dir.path()).unwrap();
    let now = store::now_secs();
    let mut n = 0u32;
    let mut admit = |op: &str, sess: &str, args: &str| {
        n += 1;
        let payload = reader.op_payload(&host, op, sess, args, &format!("{n:032x}"), now, now + 60);
        match decide(dir.path(), &reader.sign_bytes(payload.as_bytes()), now).unwrap() {
            Decision::Execute(op) => op,
            _ => panic!("{op} was not admitted"),
        }
    };
    let listed = |op: &super::envelope::Verified| {
        Store::open(dir.path())
            .unwrap()
            .readable_sessions(op, None, 50, now, None)
            .unwrap()
            .0
            .into_iter()
            .map(|s| (s.session_id, s.agent, s.cwd))
            .collect::<Vec<_>>()
    };
    let list_args = r#"{"after":null,"limit":50}"#;
    let before = admit("sessions.list", "host", list_args);
    assert!(listed(&before).is_empty());

    let id = adopt_native(
        dir.path(),
        AgentKind::OpenCode,
        native,
        Some("/work/opencode"),
    )
    .unwrap();
    assert_eq!(id, derived_session_id(AgentKind::OpenCode, native));
    assert_eq!(
        adopt_native(dir.path(), AgentKind::OpenCode, native, Some("/elsewhere")).unwrap(),
        id,
        "repeated discovery reuses the record"
    );
    let record = lookup(dir.path(), &id).unwrap().unwrap();
    assert_eq!(record.agent, AgentKind::OpenCode);
    assert_eq!(record.cwd, "/work/opencode");
    assert_eq!(record.resume_id.as_deref(), Some(native));
    assert!(adopt_native(dir.path(), AgentKind::OpenCode, "ses_other", None).is_err());

    let message = conversations_core::parse_line(&serde_json::json!({
        "type": "user", "uuid": "m1", "sessionId": native,
        "timestamp": "2026-11-02T10:00:00.000Z",
        "message": {"role": "user", "content": "opencode vault marker"}
    }))
    .unwrap();
    History::open(dir.path())
        .unwrap()
        .ingest(
            &id,
            &Source {
                agent: AgentKind::OpenCode,
                native_id: native.into(),
                origin: "/home/opencode.db".into(),
            },
            &[message],
        )
        .unwrap();

    let after = admit("sessions.list", "host", list_args);
    assert_eq!(
        listed(&after),
        vec![(
            id.clone(),
            "opencode".to_owned(),
            "/work/opencode".to_owned()
        )]
    );
    admit(
        "history.get",
        &id,
        &format!(r#"{{"session":"{id}","after":0,"limit":32}}"#),
    );
    let (items, _) = History::open(dir.path())
        .unwrap()
        .page(&id, 0, 32, 1 << 20, false)
        .unwrap();
    assert!(serde_json::to_string(&items)
        .unwrap()
        .contains("opencode vault marker"));
    admit(
        "history.search",
        &id,
        &format!(r#"{{"session":"{id}","query":"vault","limit":20}}"#),
    );
    assert_eq!(
        History::open(dir.path())
            .unwrap()
            .search("vault", Some(&id), 20)
            .unwrap()
            .len(),
        1
    );

    // A record with the canonical id bound elsewhere is refused, not retargeted.
    let claude = "9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b";
    Store::open(dir.path())
        .unwrap()
        .insert_session(
            &sessions::LocalSession {
                session_id: claude.into(),
                agent: AgentKind::Claude,
                cwd: "/w".into(),
                resume_id: Some("another-native".into()),
                launcher: Default::default(),
                adk_statusline: false,
            },
            "adopted",
        )
        .unwrap();
    assert!(adopt_native(dir.path(), AgentKind::Claude, claude, Some("/w")).is_err());
    assert_eq!(
        lookup(dir.path(), claude)
            .unwrap()
            .unwrap()
            .resume_id
            .as_deref(),
        Some("another-native")
    );
}

/// Build and sign a `pr.sync` frame under a delegation whose stored caps the
/// caller names exactly. The caller provides the parent passkey credential's
/// kid, the delegation id, the browser signing key and its public bytes; the
/// signed envelope mirrors the real browser's shape (`sess` = signed session,
/// `aud` = host id). Used by the PR-refresh regression tests below so each one
/// exercises the exact end-to-end verify + admit path.
#[allow(clippy::too_many_arguments)]
fn sign_delegated_pr_sync(
    host: &str,
    session: &str,
    browser_kid: &str,
    browser_pair: &ring::signature::EcdsaKeyPair,
    rng: &ring::rand::SystemRandom,
    dlg: &str,
    jti: &str,
    now: i64,
) -> super::envelope::SignedFrame {
    let args = format!(r#"{{"session":"{session}","owner":"octo","repo":"repo","number":7}}"#);
    let payload = format!(
        r#"{{"v":1,"typ":"op","aud":"{host}","kid":"{browser_kid}","dlg":"{dlg}","op":"pr.sync","sess":"{session}","jti":"{jti}","iat":{now},"exp":{},"args":{args}}}"#,
        now + 60
    );
    let mut message = super::envelope::DOMAIN_OP.to_vec();
    message.extend_from_slice(payload.as_bytes());
    super::envelope::SignedFrame {
        payload: payload.as_bytes().to_vec(),
        sig: browser_pair.sign(rng, &message).unwrap().as_ref().to_vec(),
        kid: browser_kid.to_owned(),
    }
}

/// The legacy four-cap owner credential observed on this laptop (see
/// `LIVE-EVIDENCE.json`) was approved with only
/// `history.read,session.reopen,session.start,session.write`. Under the current
/// rule — PR refresh follows `session.write` on the exact signed session — the
/// hosted dashboard's `pr.sync` is admitted directly: the four-cap set already
/// contains `session.write`, no local upgrade, re-enrollment or extra Touch ID
/// is needed. A standalone `session.pr_sync` grant is no longer independently
/// sufficient: a credential that holds only the obsolete cap is refused. The
/// obsolete cap stays parseable so existing ceilings and grant rows are not
/// corrupted — they just do not authorize anything on their own.
#[test]
fn legacy_four_cap_owner_admits_pr_sync_without_pr_sync_cap() {
    let dir = private_dir();
    let parent = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &parent,
        &[
            "history.read",
            "session.reopen",
            "session.start",
            "session.write",
        ],
    )
    .unwrap();
    // Passkey-derived delegations only parent off a `passkey` credential. The
    // store's admission, grant and ceiling rules are independent of the auth
    // method that originally approved the owner key.
    let db = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
    db.execute(
        "UPDATE credentials SET auth = 'passkey' WHERE cred_id = ?1",
        [&parent.kid],
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .insert_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: dir.path().to_string_lossy().into_owned(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    drop(store);
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let browser_pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let browser_public = browser_pair.public_key().as_ref().to_vec();
    let browser_kid = super::envelope::key_id(&browser_public);
    let now = store::now_secs();
    let host = host_id(dir.path()).unwrap();
    // The live-evidence delegation: four caps, no `session.pr_sync`, no extra
    // Touch ID. `session.write` is exactly what `pr.sync` now needs.
    let dlg = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    db.execute(
        "INSERT INTO delegations(dlg_id, parent_cred, browser_kid, browser_pub, alg, caps, issued_at, not_after)
         VALUES(?1, ?2, ?3, ?4, 'es256', ?5, ?6, ?7)",
        rusqlite::params![
            dlg,
            parent.kid,
            browser_kid,
            browser_public,
            "history.read,session.reopen,session.start,session.write",
            now,
            now + 3600,
        ],
    )
    .unwrap();
    let frame = sign_delegated_pr_sync(
        &host,
        SESSION,
        &browser_kid,
        &browser_pair,
        &rng,
        dlg,
        "11111111111111111111111111111111",
        now,
    );
    let decision = decide(dir.path(), &frame, now).unwrap();
    assert!(
        matches!(decision, Decision::Execute(_)),
        "a legacy four-cap owner delegation must drive PR refresh with no upgrade: {decision:?}"
    );
    // The authority carried by the admitted op is session.write, not the
    // obsolete pr_sync — so the target recorded later by the dispatch layer
    // will poll under write (which this delegation holds).
    if let Decision::Execute(verified) = decision {
        assert_eq!(verified.op.capability(), Cap::SessionWrite);
    }
}

/// A credential that only holds the obsolete `session.pr_sync` grant — and no
/// `session.write` — cannot drive PR refresh any more: standalone legacy
/// pr_sync is not an independently sufficient authority. The cap string still
/// parses (otherwise opening a database that stores it would fail), but
/// `pr.sync` enforces `session.write`.
#[test]
fn standalone_legacy_pr_sync_credential_no_longer_authorizes_pr_sync() {
    let dir = private_dir();
    let owner = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &owner,
        &[&format!("session.pr_sync:{SESSION}")],
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .insert_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: dir.path().to_string_lossy().into_owned(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    drop(store);
    let now = store::now_secs();
    let host = host_id(dir.path()).unwrap();
    let args = format!(r#"{{"session":"{SESSION}","owner":"octo","repo":"repo","number":7}}"#);
    let payload = owner.op_payload(&host, "pr.sync", SESSION, &args, JTI, now, now + 60);
    let frame = owner.sign_bytes(payload.as_bytes());
    assert!(
        matches!(
            decide(dir.path(), &frame, now).unwrap(),
            Decision::Refused(_)
        ),
        "a credential without session.write must not drive PR refresh via the obsolete cap"
    );
}

/// `session.write` for session A is not `session.write` for session B. A
/// signer may refresh only pull requests of the exact signed session.
#[test]
fn pr_sync_is_scoped_to_the_signed_session_so_write_on_a_is_not_write_on_b() {
    let dir = private_dir();
    let owner = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &owner,
        &[&format!("session.write:{SESSION}"), "session.start"],
    )
    .unwrap();
    let other = "5e1aeb93-5aba-419e-8856-a7c721406279";
    let mut store = Store::open(dir.path()).unwrap();
    for id in [SESSION, other] {
        store
            .insert_session(
                &sessions::LocalSession {
                    session_id: id.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: dir.path().to_string_lossy().into_owned(),
                    resume_id: None,
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
    }
    drop(store);
    let now = store::now_secs();
    let host = host_id(dir.path()).unwrap();
    // Admitted for the exact session this key may write to.
    let args_a = format!(r#"{{"session":"{SESSION}","owner":"octo","repo":"repo","number":7}}"#);
    let same = owner.sign_bytes(
        owner
            .op_payload(&host, "pr.sync", SESSION, &args_a, JTI, now, now + 60)
            .as_bytes(),
    );
    assert!(matches!(
        decide(dir.path(), &same, now).unwrap(),
        Decision::Execute(_)
    ));
    // Cross-session: the signed session changes to one the signer does not
    // hold write on, and admission refuses it. A fresh jti avoids replay.
    let args_b = format!(r#"{{"session":"{other}","owner":"octo","repo":"repo","number":7}}"#);
    let cross = owner.sign_bytes(
        owner
            .op_payload(
                &host,
                "pr.sync",
                other,
                &args_b,
                "22222222222222222222222222222222",
                now,
                now + 60,
            )
            .as_bytes(),
    );
    assert!(
        matches!(
            decide(dir.path(), &cross, now).unwrap(),
            Decision::Refused(_)
        ),
        "a writer on session A must not be able to refresh pull requests of session B"
    );
}

/// A read-only credential (only `history.read`) cannot refresh pull requests:
/// write access is the boundary that gates both terminal input and PR refresh.
#[test]
fn pr_sync_refuses_history_read_only_credential() {
    let dir = private_dir();
    let owner = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &owner,
        &["history.read", "session.start"],
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .insert_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: dir.path().to_string_lossy().into_owned(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    drop(store);
    let now = store::now_secs();
    let host = host_id(dir.path()).unwrap();
    let args = format!(r#"{{"session":"{SESSION}","owner":"octo","repo":"repo","number":7}}"#);
    let frame = owner.sign_bytes(
        owner
            .op_payload(&host, "pr.sync", SESSION, &args, JTI, now, now + 60)
            .as_bytes(),
    );
    assert!(
        matches!(
            decide(dir.path(), &frame, now).unwrap(),
            Decision::Refused(_)
        ),
        "a read-only viewer must not be able to drive PR refresh"
    );
}

/// On initialization, remembered PR targets whose recording authority was the
/// obsolete `session.pr_sync` cap are migrated to `session.write`, so the
/// background poll re-authorizes them against the recording credential's
/// CURRENT `session.write` grant. A credential that no longer has
/// `session.write` loses the target; a credential that still has it keeps it.
#[test]
fn migration_rewrites_recorded_pr_target_cap_from_legacy_pr_sync() {
    let dir = private_dir();
    let owner = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &owner,
        &[
            "history.read",
            "session.reopen",
            "session.start",
            "session.write",
        ],
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .insert_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: dir.path().to_string_lossy().into_owned(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    drop(store);
    // Legacy pr_sync and write grants have distinct activation epochs.
    let now = store::now_secs();
    let conn = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
    conn.execute(
        "INSERT INTO grants(cred_id,session_id,cap,granted_at) VALUES(?1,'*','session.pr_sync',?2)",
        rusqlite::params![owner.kid, now],
    )
    .unwrap();
    let epoch: i64 = conn
        .query_row(
            "SELECT epoch FROM grants WHERE cred_id=?1 AND cap='session.pr_sync'",
            [&owner.kid],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO session_pr_targets(session_id, owner, repo, number, added_at, kid, dlg, cap, epoch)
         VALUES(?1, 'octo', 'repo', 7, ?2, ?3, NULL, 'session.pr_sync', ?4)",
        rusqlite::params![SESSION, now, owner.kid, epoch],
    )
    .unwrap();
    conn.execute(
        "UPDATE credentials SET auth='passkey' WHERE cred_id=?1",
        [&owner.kid],
    )
    .unwrap();
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    let rng = ring::rand::SystemRandom::new();
    for (number, caps, expires, revoked) in [
        (8, "session.write", now + 3600, None),
        (9, "session.write", now + 3600, Some(now)),
        (10, "session.write", now - 1, None),
        (11, "history.read", now + 3600, None),
    ] {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let public = pair.public_key().as_ref();
        let kid = super::envelope::key_id(public);
        let dlg = format!("migration-{number}");
        conn.execute(
            "INSERT INTO delegations(dlg_id,parent_cred,browser_kid,browser_pub,alg,caps,issued_at,not_after,revoked_at)
             VALUES(?1,?2,?3,?4,'es256',?5,?6,?7,?8)",
            rusqlite::params![dlg,owner.kid,kid,public,caps,now,expires,revoked],
        ).unwrap();
        conn.execute(
            "INSERT INTO session_pr_targets(session_id,owner,repo,number,added_at,kid,dlg,cap,epoch)
             VALUES(?1,'octo','repo',?2,?3,?4,?5,'session.pr_sync',?6)",
            rusqlite::params![SESSION,number,now,kid,dlg,epoch],
        ).unwrap();
    }
    conn.pragma_update(None, "user_version", 12).unwrap();
    drop(conn);
    // Re-open: the migration runs.
    let store = Store::open(dir.path()).unwrap();
    let migrated: String = store
        .connection()
        .query_row(
            "SELECT cap FROM session_pr_targets WHERE session_id = ?1 AND number=7",
            [SESSION],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(migrated, Cap::SessionWrite.as_str());
    // Polling re-authorizes under the recording credential's CURRENT grants:
    // the owner still has `session.write`, so the target is returned.
    let targets = store.pr_targets(SESSION, now).unwrap();
    assert_eq!(
        targets,
        vec![
            ("octo".to_owned(), "repo".to_owned(), 7i64),
            ("octo".to_owned(), "repo".to_owned(), 8i64),
        ],
        "only the direct and live write delegation survive migration"
    );
    store
        .connection()
        .execute(
            "UPDATE grants SET revoked_at=?1 WHERE cap='session.write'",
            [now],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "UPDATE grants SET revoked_at=NULL WHERE cap='session.write'",
            [],
        )
        .unwrap();
    assert!(
        store.pr_targets(SESSION, now).unwrap().is_empty(),
        "migration binds the current write activation, never a later re-grant"
    );
}

/// Per-target gradual backoff: a target whose metadata is unchanged doubles
/// its delay every successful poll, up to the ceiling; a change resets it to
/// the floor; a transient failure doubles the delay without clobbering the
/// last-seen fingerprint so the NEXT good read can still detect
/// "changed-since-last-success". Each target's schedule is independent of
/// its neighbours.
#[test]
fn pr_poll_backoff_grows_until_a_change_and_isolates_per_target() {
    use super::store::{PR_POLL_MAX_DELAY_SECS, PR_POLL_MIN_DELAY_SECS};
    let dir = private_dir();
    let owner = Signer::new();
    enroll(
        dir.path(),
        "owner",
        &owner,
        &[&format!("session.write:{SESSION}"), "session.start"],
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .insert_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: dir.path().to_string_lossy().into_owned(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    let now = store::now_secs();
    let authority_template = super::cutover::Authority {
        kid: owner.kid.clone(),
        dlg: None,
        session: SESSION.into(),
        cap: Cap::SessionWrite.as_str().into(),
        epoch: None,
        exp: None,
    };
    let authority = super::cutover::Authority {
        epoch: store.current_epoch(&authority_template, now).unwrap(),
        ..authority_template
    };

    // Reads a target's on-disk (next_poll_at - now, current_delay_secs, fingerprint)
    // so the test can assert each target's state independently.
    let state = |store: &Store, number: i64, at: i64| -> (i64, i64, Option<String>) {
        store
            .connection()
            .query_row(
                "SELECT next_poll_at, current_delay_secs, fingerprint
                 FROM session_pr_targets
                 WHERE session_id = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
                rusqlite::params![SESSION, "octo", "repo", number],
                |r| Ok((r.get::<_, i64>(0)? - at, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    };

    store
        .record_pr_target(SESSION, "octo", "repo", 7, Some(&authority), now)
        .unwrap();
    store
        .record_pr_target(SESSION, "octo", "repo", 8, Some(&authority), now)
        .unwrap();

    // Both targets are due immediately on first record.
    let due: Vec<_> = store
        .pr_targets_poll_candidates(SESSION, now, false)
        .unwrap()
        .into_iter()
        .map(|(o, r, n, _)| (o, r, n))
        .collect();
    assert_eq!(
        due,
        vec![
            ("octo".into(), "repo".into(), 7i64),
            ("octo".into(), "repo".into(), 8i64),
        ],
    );

    // Unchanged poll of #7 doubles its delay; #8 is not touched.
    store
        .apply_pr_poll_outcome(SESSION, "octo", "repo", 7, Some("fp-a"), false, now)
        .unwrap();
    let (delta_7, delay_7, _) = state(&store, 7, now);
    assert_eq!(
        (delta_7, delay_7),
        (PR_POLL_MIN_DELAY_SECS * 2, PR_POLL_MIN_DELAY_SECS * 2),
        "first unchanged outcome must double the floor",
    );
    let due_at_t0: Vec<_> = store
        .pr_targets_poll_candidates(SESSION, now, false)
        .unwrap()
        .into_iter()
        .map(|(_, _, n, _)| n)
        .collect();
    assert_eq!(
        due_at_t0,
        vec![8i64],
        "#8 must stay independently due while #7 backs off",
    );

    // Walk the schedule forward until it reaches the ceiling. Advancing wall
    // time in sync with the growing delay simulates the daemon waking at each
    // target's own schedule, not on every neighbour's tick.
    let mut t = now + PR_POLL_MIN_DELAY_SECS * 2;
    let mut expected_delay = PR_POLL_MIN_DELAY_SECS * 2;
    while expected_delay < PR_POLL_MAX_DELAY_SECS {
        store
            .apply_pr_poll_outcome(SESSION, "octo", "repo", 7, Some("fp-a"), false, t)
            .unwrap();
        expected_delay = (expected_delay * 2).min(PR_POLL_MAX_DELAY_SECS);
        let (_, delay, fp) = state(&store, 7, t);
        assert_eq!(delay, expected_delay);
        assert_eq!(fp.as_deref(), Some("fp-a"));
        t += expected_delay;
    }
    assert_eq!(
        expected_delay, PR_POLL_MAX_DELAY_SECS,
        "repeated unchanged outcomes must cap at the ceiling",
    );
    // Further unchanged outcomes stay AT the ceiling.
    store
        .apply_pr_poll_outcome(SESSION, "octo", "repo", 7, Some("fp-a"), false, t)
        .unwrap();
    assert_eq!(state(&store, 7, t).1, PR_POLL_MAX_DELAY_SECS);

    // A changed poll of #7 resets its delay back to the floor.
    store
        .apply_pr_poll_outcome(SESSION, "octo", "repo", 7, Some("fp-b"), true, t)
        .unwrap();
    let (_, delay, fp) = state(&store, 7, t);
    assert_eq!(
        (delay, fp.as_deref()),
        (PR_POLL_MIN_DELAY_SECS, Some("fp-b")),
        "a change must reset to the floor and record the new fingerprint",
    );

    // Transient failure: pass None fingerprint. The delay doubles but the
    // last-seen fingerprint is preserved so a later good read still sees the
    // change against "fp-b".
    store
        .apply_pr_poll_outcome(SESSION, "octo", "repo", 7, None, false, t)
        .unwrap();
    let (_, delay, fp) = state(&store, 7, t);
    assert_eq!(
        (delay, fp.as_deref()),
        (PR_POLL_MIN_DELAY_SECS * 2, Some("fp-b")),
        "transient failure must double without clobbering the last good fingerprint",
    );

    // A new signed pr.sync (record_pr_target with the same key) is the
    // "manual bypass" lane: it resets the target's schedule and clears the
    // last-seen fingerprint so the next poll fires immediately at the floor.
    let later = t + 10;
    store
        .record_pr_target(SESSION, "octo", "repo", 7, Some(&authority), later)
        .unwrap();
    let (delta, delay, fp) = state(&store, 7, later);
    assert_eq!(
        (delay, fp),
        (PR_POLL_MIN_DELAY_SECS, None),
        "manual bypass must reset to the floor and clear the fingerprint",
    );
    assert!(
        delta <= 0,
        "manual bypass must make the target due immediately (delta={delta})",
    );

    // A target first recorded AFTER another target is deep in backoff does
    // NOT inherit that neighbour's stale delay: new targets start at the
    // floor regardless of their siblings' state.
    store
        .apply_pr_poll_outcome(SESSION, "octo", "repo", 8, Some("fp-8"), false, later)
        .unwrap();
    store
        .apply_pr_poll_outcome(SESSION, "octo", "repo", 8, Some("fp-8"), false, later)
        .unwrap();
    store
        .record_pr_target(SESSION, "octo", "repo", 9, Some(&authority), later)
        .unwrap();
    let (delta_9, delay_9, fp_9) = state(&store, 9, later);
    assert_eq!(
        (delay_9, fp_9),
        (PR_POLL_MIN_DELAY_SECS, None),
        "a new target starts at the floor, not inheriting siblings' delay",
    );
    assert!(
        delta_9 <= 0,
        "a new target must be due immediately on first record",
    );
}

/// Owner-only caps must never land on an agent credential through the local
/// upgrade path, even when a caller passes a known agent key id.
#[test]
fn grant_owner_only_caps_refuses_agent_credentials_and_revoked_keys() {
    let dir = private_dir();
    let owner = Signer::new();
    enroll(dir.path(), "owner", &owner, &["session.start"]).unwrap();
    let agent = Signer::new();
    enroll(
        dir.path(),
        "agent",
        &agent,
        &[&format!("session.write:{SESSION}")],
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let agent_refusal = store
        .grant_owner_only_caps(&agent.kid, &[Cap::PrSync])
        .unwrap_err()
        .to_string();
    assert!(
        agent_refusal.contains("owner credential"),
        "{agent_refusal}"
    );
    store
        .connection()
        .execute(
            "UPDATE credentials SET revoked_at = 1 WHERE cred_id = ?1",
            [&owner.kid],
        )
        .unwrap();
    let revoked = store
        .grant_owner_only_caps(&owner.kid, &[Cap::PrSync])
        .unwrap_err()
        .to_string();
    assert!(revoked.contains("revoked"), "{revoked}");
}

#[test]
fn uncertain_imported_continuation_cannot_fork_with_a_new_operation() {
    let dir = private_dir();
    let owner = Signer::new();
    let host = enroll(dir.path(), "owner", &owner, &["session.reopen"]).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .insert_imported_session(
            &sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Codex,
                cwd: dir.path().to_string_lossy().into_owned(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "continuation",
        )
        .unwrap();
    let now = store::now_secs();
    let first = owner.sign_bytes(
        owner
            .reopen_payload(&host, SESSION, JTI, now, now + 60)
            .as_bytes(),
    );
    let Decision::Execute(op) = decide(dir.path(), &first, now).unwrap() else {
        panic!("first continuation must run")
    };
    begin(dir.path(), &op).unwrap();
    recover(dir.path()).unwrap();
    assert!(
        matches!(decide(dir.path(), &first, now).unwrap(), Decision::Replay { state, .. } if state == "uncertain")
    );
    let second = owner.sign_bytes(
        owner
            .reopen_payload(
                &host,
                SESSION,
                "ffffffffffffffffffffffffffffffff",
                now,
                now + 60,
            )
            .as_bytes(),
    );
    assert!(
        matches!(decide(dir.path(), &second, now).unwrap(), Decision::Refused(reason) if reason.contains("continuation"))
    );
    // Once a native identity is established, ordinary reopen is safe: it resumes
    // that identity rather than creating another fresh session.
    store.bind_resume_once(SESSION, "native-confirmed").unwrap();
    assert!(matches!(
        decide(dir.path(), &second, now).unwrap(),
        Decision::Execute(_)
    ));
}
