#[test]
fn named_links_survive_repaint_and_server_snapshot() {
    let uri = "https://linear.app/example/project/example-project";
    let bytes = format!("\x1b]8;;{uri}\x1b\\Secondary OIDC trust\x1b]8;;\x1b\\ plain");
    let mut host = vt100::Parser::new(6, 80, 100);
    host.process(bytes.as_bytes());
    let repaint = host.screen().contents_formatted();
    assert!(
        String::from_utf8_lossy(&repaint).contains(uri),
        "host repaint lost the link target"
    );
    let mut server = vt100::Parser::new(6, 80, 100);
    server.process(&repaint);
    let snapshot = server.screen().contents_formatted();
    assert!(
        String::from_utf8_lossy(&snapshot).contains(uri),
        "server snapshot lost the link target"
    );
    assert_eq!(server.screen().contents(), "Secondary OIDC trust plain");
    for col in 0..19 {
        assert_eq!(server.screen().cell(0, col).unwrap().hyperlink(), Some(uri));
    }
    assert_eq!(server.screen().cell(0, 20).unwrap().hyperlink(), None);
}

fn replay(bytes: &[u8], cols: u16) -> vt100::Parser {
    let mut source = vt100::Parser::new(6, cols, 100);
    for chunk in bytes.chunks(3) {
        source.process(chunk);
    }
    let mut viewer = vt100::Parser::new(6, cols, 100);
    viewer.process(&source.screen().contents_formatted());
    viewer
}

#[test]
fn wrapped_unicode_links_keep_targets_after_style_reset_and_replay() {
    let viewer = replay(
        b"\x1b]8;id=project;https://example.com/a;b\x07\xe7\x95\x8c\x1b[0mabcdef\x1b]8;;\x07 plain",
        6,
    );
    let screen = viewer.screen();
    assert_eq!(
        screen.cell(0, 0).unwrap().hyperlink(),
        Some("https://example.com/a;b")
    );
    assert_eq!(
        screen.cell(1, 0).unwrap().hyperlink(),
        Some("https://example.com/a;b")
    );
    assert_eq!(screen.cell(1, 3).unwrap().hyperlink(), None);
}

#[test]
fn overwrite_and_erase_remove_stale_targets() {
    let viewer = replay(
        b"\x1b]8;;https://example.com\x1b\\linked\x1b]8;;\x1b\\\rplain\x1b[K",
        80,
    );
    assert_eq!(viewer.screen().contents(), "plain");
    for col in 0..80 {
        assert_eq!(viewer.screen().cell(0, col).unwrap().hyperlink(), None);
    }
}

#[test]
fn repaint_closes_receiver_link_before_plain_text() {
    let mut source = vt100::Parser::new(6, 80, 100);
    source.process(b"plain");
    let mut viewer = vt100::Parser::new(6, 80, 100);
    viewer.process(b"\x1b]8;;https://stale.example.com\x1b\\old");
    viewer.process(&source.screen().contents_formatted());
    assert_eq!(viewer.screen().cell(0, 0).unwrap().hyperlink(), None);
}

#[test]
fn standalone_row_does_not_link_later_live_output() {
    let mut host = vt100::Parser::new(1, 7, 0);
    host.process(b"\x1b]8;;https://example.com/project\x1b\\Project\x1b]8;;\x1b\\");
    let row = host.screen().rows_formatted(0, 7).next().unwrap();
    let mut viewer = vt100::Parser::new(2, 7, 0);
    viewer.process(&row);
    viewer.process(b"\x1b[m\r\nplain");
    assert_eq!(
        viewer.screen().cell(0, 0).unwrap().hyperlink(),
        Some("https://example.com/project")
    );
    assert_eq!(viewer.screen().cell(1, 0).unwrap().hyperlink(), None);
}
