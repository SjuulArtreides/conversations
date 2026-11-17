//! Opt-in real shared-SPA proof. All host, browser and home state is temporary.
use super::super::{
    authz::{
        sessions::LocalSession,
        store::{Grant, Kind},
    },
    history::{History, Source},
    protocol::{Frame, Launch, Registration, SessionStatus},
    Backend,
};
use super::*;
use conversations_core::{terminal::PtyFrameKind, AgentKind, AgentLauncher};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::Mutex,
};

const EXACT: &str = "11111111-1111-4111-8111-111111111111";
const PARTIAL: &str = "22222222-2222-4222-8222-222222222222";
const TEST: &str = "commands::local_host::web::ui_tests::shared_spa_headless_owner_journey";

#[test]
#[ignore = "requires PRIVATE_TRANSPORT_CHROME and PRIVATE_TRANSPORT_NODE; isolated real Chromium shared-SPA journey"]
fn shared_spa_headless_owner_journey() {
    // Re-exec only this test with a private home/config: no process-global environment
    // changes race other tests, and no real user settings or credentials are consulted.
    if std::env::var_os("LOCAL_UI_PROOF_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", TEST, "--nocapture"])
            .env("LOCAL_UI_PROOF_CHILD", "1")
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env(
                "CONVERSATIONS_HOST_SETTINGS_FILE",
                home.path().join("settings.json"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated UI test failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(
        std::env::var_os("PRIVATE_TRANSPORT_CHROME").is_some(),
        "set PRIVATE_TRANSPORT_CHROME"
    );
    let node = std::env::var("PRIVATE_TRANSPORT_NODE").expect("set PRIVATE_TRANSPORT_NODE");
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap().block_on(async move {
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::open(runtime.path()).unwrap();
        store.host_id().unwrap();
        store.enroll(Kind::Owner, Alg::Ed25519, &[7;32], "QA browser key", &[Grant::parse("history.read").unwrap()]).unwrap();
        let mut history = History::open(runtime.path()).unwrap();
        for (id,title,at) in [(EXACT,"Atlas","2026-01-01T10:00:00Z"),(PARTIAL,"Atlas follow-up","2026-11-01T10:00:00Z")] {
            store.insert_session(&LocalSession {session_id:id.into(),agent:AgentKind::Claude,cwd:"/fixture/project".into(),resume_id:Some(id.into()),launcher:AgentLauncher::Opencode,adk_statusline:false},"adopted").unwrap();
            let message = conversations_core::parse_line(&json!({"type":"user","uuid":format!("message-{id}"),"sessionId":id,"timestamp":at,"message":{"role":"user","content":"Harmless isolated UI fixture transcript."}})).unwrap();
            history.import_messages(id,&Source {agent:AgentKind::Claude,native_id:id.into(),origin:"explicit-remote-import".into()},&[message]).unwrap();
            history.rename(id,title).unwrap();
        }
        let inputs=Arc::new(Mutex::new(Vec::<u8>::new()));
        let unix=tokio::net::UnixListener::bind(runtime.path().join("host.sock")).unwrap();
        let captured=inputs.clone();
        let fake=tokio::spawn(async move {
            while let Ok((mut stream,_))=unix.accept().await {
                let captured=captured.clone();
                tokio::spawn(async move {
                    match read_message::<_,ClientMessage>(&mut stream).await.unwrap() {
                        ClientMessage::Status => {
                            let sessions=[EXACT,PARTIAL].into_iter().map(|id|SessionStatus {native_session_id:None,native_identity_supported:true,readiness:None,registration:Registration {launch:Launch {id:id.into(),agent:AgentKind::Claude,cwd:"/fixture/project".into(),command:vec!["fake-agent-never-executed".into()]},workspace:None,surface:None,local_terminal:true,reconnecting:false,file_receiver:false,transfer_gate:false,effect_gate:true,in_place:false},local_input_bytes:0,local_input_dropped_chunks:0}).collect();
                            write_message(&mut stream,&ServerMessage::Status {backend:Backend::Normal,sessions,in_place_sessions:true}).await.unwrap();
                        }
                        ClientMessage::LocalTerminal {id} => {
                            assert!([EXACT,PARTIAL].contains(&id.as_str()));
                            let frame=||ServerMessage::TerminalFrame {frame:Frame {cols:100,rows:30,b64:STANDARD.encode("\x1b[2J\x1b[HIsolated shared-SPA terminal fixture\r\n$ "),kind:PtyFrameKind::Snapshot}};
                            write_message(&mut stream,&frame()).await.unwrap();
                            while let Ok(input)=read_message::<_,LocalTerminalInput>(&mut stream).await {
                                match input {
                                    LocalTerminalInput::Input {b64} => captured.lock().unwrap().extend(STANDARD.decode(b64).unwrap()),
                                    LocalTerminalInput::Repaint|LocalTerminalInput::Resize {..} => {if write_message(&mut stream,&frame()).await.is_err() {break;}},
                                }
                            }
                        }
                        _ => {let _=write_message(&mut stream,&ServerMessage::Error {message:"No actions are executed by the UI fixture".into()}).await;}
                    }
                });
            }
        });
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port=listener.local_addr().unwrap().port();
        let state=WebState::new(runtime.path(),"http://127.0.0.1:1",port).unwrap();
        let bootstrap=state.rotate_bootstrap_url().unwrap();
        let token=bootstrap.rsplit("#token=").next().unwrap().to_owned();
        let config=json!({"base":format!("http://127.0.0.1:{port}"),"token":token,"profile":runtime.path().join("chrome"),"output":Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/proof/local-ui")});
        let server=tokio::spawn(async move {axum::serve(listener,router(state)).await.unwrap();});
        let script=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/scripts/local-ui-proof.mjs");
        let result=tokio::task::spawn_blocking(move || {
            let mut child=Command::new(node).arg(script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(config.to_string().as_bytes()).unwrap();
            child.wait_with_output().unwrap()
        }).await.unwrap();
        server.abort(); fake.abort();
        assert!(result.status.success(),"shared SPA assertions failed: {}",String::from_utf8_lossy(&result.stdout));
        assert!(String::from_utf8_lossy(&inputs.lock().unwrap()).contains("UI_PROOF_INPUT"),"browser terminal input never reached the fake Unix PTY");
        let report_path=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/proof/local-ui/assertions.json");
        let mut report:Value=serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
        report["assertions"].as_array_mut().unwrap().push(json!("browser terminal input reached fake Unix PTY"));
        std::fs::write(report_path,serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    });
}
