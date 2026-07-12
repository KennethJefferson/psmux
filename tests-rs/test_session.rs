// Tests for crate::session::fetch_session_info, covering the AUTH+session-info
// framing race that motivated issue #250.
//
// Each test spins up a minimal in-process TCP listener on 127.0.0.1:0 that
// acts as a fake psmux session server, then calls the real production
// function — no re-implementation of the parser in the test.

use super::*;

use std::fs;
use std::io::{Read, Write as IoWrite};
use std::path::PathBuf;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Read the `AUTH <key>\n` + `session-info\n` lines the client sends so the
/// fake server's subsequent writes land against the expected client state.
fn drain_client_request(stream: &mut TcpStream) {
    // AUTH line + session-info line — two LFs total.
    let mut seen_lf = 0u8;
    let mut buf = [0u8; 1];
    while seen_lf < 2 {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {
                if buf[0] == b'\n' {
                    seen_lf += 1;
                }
            }
            Err(_) => return,
        }
    }
}

/// Spawns a listener bound to an ephemeral port, hands the accepted stream
/// to `respond`, and returns `127.0.0.1:<port>` for the client to dial.
///
/// Returns the address plus a channel the caller can block on to ensure the
/// server thread finished before the test exits.
fn spawn_fake_server<F>(respond: F) -> (String, mpsc::Receiver<()>)
where
    F: FnOnce(TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap().to_string();
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            respond(stream);
        }
        let _ = done_tx.send(());
    });
    (addr, done_rx)
}

fn temp_psmux_dir(test_name: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("psmux_{test_name}_{}_{}", std::process::id(), n))
        .join(".psmux");
    let _ = fs::remove_dir_all(dir.parent().unwrap());
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_registry_files(
    dir: &std::path::Path,
    session: &str,
    port: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let port_path = dir.join(format!("{session}.port"));
    let key_path = dir.join(format!("{session}.key"));
    let sid_path = dir.join(format!("{session}.sid"));
    fs::write(&port_path, port).unwrap();
    fs::write(&key_path, "test-key").unwrap();
    fs::write(&sid_path, "7").unwrap();
    (port_path, key_path, sid_path)
}

#[test]
fn happy_path_returns_info_line() {
    let (addr, done) = spawn_fake_server(|mut s| {
        drain_client_request(&mut s);
        let _ = s.write_all(b"OK\n");
        let _ = s.write_all(b"call-controller: 2 windows (created Mon Apr 20 11:10:58 2026)\n");
        let _ = s.flush();
    });

    let info = fetch_session_info(
        &addr,
        "key",
        Duration::from_millis(200),
        Duration::from_millis(500),
    );

    assert_eq!(
        info.as_deref(),
        Some("call-controller: 2 windows (created Mon Apr 20 11:10:58 2026)")
    );
    let _ = done.recv_timeout(Duration::from_secs(2));
}

#[test]
fn issue_250_late_auth_ack_is_not_reported_as_session_info() {
    // Reproduces the #250 race: AUTH `OK\n` is delayed until after the client's
    // first read_line would have timed out. In the old code the late "OK"
    // landed in the second read and was rendered as the session name. The
    // production function must either return the real info or `None` — never
    // `Some("OK")`.
    let (addr, done) = spawn_fake_server(|mut s| {
        drain_client_request(&mut s);
        // Hold the "OK" ack longer than the client's per-read timeout so the
        // first read_line is forced to return (on the old code, empty) and
        // the ack arrives during what was previously the "info" read.
        thread::sleep(Duration::from_millis(120));
        let _ = s.write_all(b"OK\n");
        let _ = s.flush();
        // Then send the real info line comfortably within the second read.
        thread::sleep(Duration::from_millis(20));
        let _ = s.write_all(b"convserv: 3 windows (created Mon Apr 20 11:11:06 2026)\n");
        let _ = s.flush();
    });

    let info = fetch_session_info(
        &addr,
        "key",
        Duration::from_millis(200),
        Duration::from_millis(80),  // shorter than the 120ms server delay
    );

    // The critical assertion: even under the race, we never mis-report "OK"
    // as the info line. Either the real line makes it (if the read timeout
    // is generous) or we get None — but never Some("OK").
    assert_ne!(info.as_deref(), Some("OK"), "late AUTH ack leaked as session info");
    let _ = done.recv_timeout(Duration::from_secs(2));
}

#[test]
fn only_ok_ack_received_returns_none() {
    // Server replies with just the AUTH ack and never sends session-info
    // (the worst-case of #250: second read's timeout leaves nothing).
    let (addr, done) = spawn_fake_server(|mut s| {
        drain_client_request(&mut s);
        let _ = s.write_all(b"OK\n");
        let _ = s.flush();
        // Keep the connection open briefly so the client isn't racing EOF
        // against its own read_timeout.
        thread::sleep(Duration::from_millis(200));
    });

    let info = fetch_session_info(
        &addr,
        "key",
        Duration::from_millis(200),
        Duration::from_millis(80),
    );

    assert_eq!(info, None, "sole OK ack must not be reported as info");
    let _ = done.recv_timeout(Duration::from_secs(2));
}

#[test]
fn connect_refused_returns_none() {
    // Bind then drop the listener so the port is (briefly) closed — on
    // loopback this produces a fast refusal. The socket might race to be
    // reused, but `fetch_session_info` must never panic and must return
    // None on connect failure.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);

    let info = fetch_session_info(
        &addr,
        "key",
        Duration::from_millis(50),
        Duration::from_millis(50),
    );

    assert_eq!(info, None);
}

#[test]
fn auth_rejected_returns_none() {
    // Server responds to AUTH with an error instead of OK — must not be
    // rendered as the session info line.
    let (addr, done) = spawn_fake_server(|mut s| {
        drain_client_request(&mut s);
        let _ = s.write_all(b"ERROR: Invalid session key\n");
        let _ = s.flush();
    });

    let info = fetch_session_info(
        &addr,
        "wrong-key",
        Duration::from_millis(200),
        Duration::from_millis(200),
    );

    // The picker should fall back to the generic "(not responding)"
    // label rather than rendering the raw ERROR line as the session info.
    assert_eq!(info, None, "auth error leaked as session info: {:?}", info);
    let _ = done.recv_timeout(Duration::from_secs(2));
}

#[test]
fn stale_cleanup_removes_invalid_port_and_key() {
    let dir = temp_psmux_dir("stale_cleanup_invalid");
    let (port_path, key_path, sid_path) = write_registry_files(&dir, "bad", "not-a-port");

    cleanup_stale_port_files_in(&dir);

    assert!(!port_path.exists(), "invalid .port file should be removed");
    assert!(!key_path.exists(), "matching .key file should be removed");
    assert!(!sid_path.exists(), "matching .sid file should be removed");
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[test]
fn stale_cleanup_removes_registry_only_when_probe_confirms_stale() {
    let dir = temp_psmux_dir("stale_cleanup_confirmed");
    let (port_path, key_path, sid_path) = write_registry_files(&dir, "dead", "54321");

    cleanup_stale_port_files_in_with(&dir, |_, _| PortProbeResult::Stale);

    assert!(!port_path.exists(), "confirmed-stale .port file should be removed");
    assert!(!key_path.exists(), "matching .key file should be removed");
    assert!(!sid_path.exists(), "matching .sid file should be removed");
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[test]
fn stale_cleanup_preserves_registry_when_probe_is_inconclusive() {
    let dir = temp_psmux_dir("stale_cleanup_inconclusive");
    let (port_path, key_path, sid_path) =
        write_registry_files(&dir, "maybe-live", "54322");

    cleanup_stale_port_files_in_with(&dir, |_, _| PortProbeResult::Inconclusive);

    assert!(port_path.exists(), "inconclusive probe must not remove .port");
    assert!(key_path.exists(), "inconclusive probe must not remove .key");
    assert!(sid_path.exists(), "inconclusive probe must not remove .sid");
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[test]
fn stale_cleanup_preserves_registry_for_live_listener() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind live listener");
    let port = listener.local_addr().unwrap().port().to_string();
    let dir = temp_psmux_dir("stale_cleanup_live");
    let (port_path, key_path, sid_path) = write_registry_files(&dir, "live", &port);

    cleanup_stale_port_files_in(&dir);

    assert!(port_path.exists(), "live listener .port should be preserved");
    assert!(key_path.exists(), "live listener .key should be preserved");
    assert!(sid_path.exists(), "live listener .sid should be preserved");
    drop(listener);
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

/// Read a single `\n`-terminated line from the stream (the probe's AUTH line),
/// so the fake server's reply lands against the client's read.
fn read_one_line(stream: &mut TcpStream) {
    let mut buf = [0u8; 1];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(_) if buf[0] == b'\n' => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

#[test]
fn stale_cleanup_removes_session_when_port_reused_by_other_server() {
    // After a crash/reboot the old port can be grabbed by a *different* live
    // psmux server, which rejects our key. A bare TCP connect would call this
    // "alive" and leave the dead session as a "(not responding)" zombie; the
    // identity probe must classify the key rejection as Stale and reap it.
    let (addr, done) = spawn_fake_server(|mut s| {
        read_one_line(&mut s);
        let _ = s.write_all(b"ERROR: Invalid session key\n");
        let _ = s.flush();
    });
    let port = addr.rsplit(':').next().unwrap().to_string();
    let dir = temp_psmux_dir("stale_cleanup_reused_port");
    let (port_path, key_path, sid_path) = write_registry_files(&dir, "ghost", &port);

    cleanup_stale_port_files_in(&dir);

    assert!(!port_path.exists(), "key-rejected (reused) .port must be removed");
    assert!(!key_path.exists(), "matching .key must be removed");
    assert!(!sid_path.exists(), "matching .sid must be removed");
    let _ = done.recv_timeout(Duration::from_secs(2));
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[test]
fn stale_cleanup_preserves_session_for_authenticated_server() {
    // Our own live server accepts the key — it must never be reaped.
    let (addr, done) = spawn_fake_server(|mut s| {
        read_one_line(&mut s);
        let _ = s.write_all(b"OK\n");
        let _ = s.flush();
        thread::sleep(Duration::from_millis(100));
    });
    let port = addr.rsplit(':').next().unwrap().to_string();
    let dir = temp_psmux_dir("stale_cleanup_authed");
    let (port_path, key_path, sid_path) = write_registry_files(&dir, "mine", &port);

    cleanup_stale_port_files_in(&dir);

    assert!(port_path.exists(), "authenticated .port must be preserved");
    assert!(key_path.exists(), "authenticated .key must be preserved");
    assert!(sid_path.exists(), "authenticated .sid must be preserved");
    let _ = done.recv_timeout(Duration::from_secs(2));
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[test]
fn pre_boot_registry_is_reaped_regardless_of_port() {
    use std::time::SystemTime;
    let boot = SystemTime::now();
    let margin = Duration::from_secs(10);

    // Written well before boot (previous boot) -> reap.
    let old = boot - Duration::from_secs(3600);
    assert!(is_pre_boot(old, boot, margin), "pre-boot file must be reaped");

    // Written within the boot grace window -> keep (could be a server that
    // came up moments after boot).
    let recent = boot - Duration::from_secs(2);
    assert!(!is_pre_boot(recent, boot, margin), "just-after-boot file must be kept");

    // Written after boot -> keep.
    let fresh = boot + Duration::from_secs(30);
    assert!(!is_pre_boot(fresh, boot, margin), "post-boot file must be kept");
}

#[test]
fn liveness_authenticated_server_is_alive() {
    let (addr, done) = spawn_fake_server(|mut s| {
        drain_client_request(&mut s); // AUTH + session-info (two lines)
        let _ = s.write_all(b"OK\n");
        let _ = s.write_all(b"mysession: 2 windows (created Mon Apr 20 11:10:58 2026)\n");
        let _ = s.flush();
        thread::sleep(Duration::from_millis(50));
    });

    let v = probe_session_liveness(
        &addr,
        "key",
        Duration::from_millis(300),
        Duration::from_millis(400),
    );

    match v {
        SessionLiveness::Alive(info) => assert!(info.contains("mysession"), "info: {info}"),
        other => panic!("expected Alive, got {other:?}"),
    }
    let _ = done.recv_timeout(Duration::from_secs(2));
}

#[test]
fn liveness_auth_rejection_is_dead() {
    // The reboot/reused-port case: a different server rejects our key.
    let (addr, done) = spawn_fake_server(|mut s| {
        drain_client_request(&mut s);
        let _ = s.write_all(b"ERROR: Invalid session key\n");
        let _ = s.flush();
    });

    let v = probe_session_liveness(
        &addr,
        "stale-key",
        Duration::from_millis(300),
        Duration::from_millis(400),
    );

    assert_eq!(v, SessionLiveness::Dead, "auth rejection must be Dead");
    let _ = done.recv_timeout(Duration::from_secs(2));
}

#[test]
fn liveness_connection_refused_is_dead() {
    // Bind then drop so the port is guaranteed free -> connect refused.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to grab a port");
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);

    let v = probe_session_liveness(
        &addr,
        "key",
        Duration::from_millis(300),
        Duration::from_millis(200),
    );

    assert_eq!(v, SessionLiveness::Dead, "refused connect must be Dead");
}

#[test]
fn liveness_connected_but_silent_is_dead() {
    // A listener that accepts (via backlog) but never speaks our protocol.
    // Bounded: we wait one read timeout, then declare it Dead (honors
    // "no response within the timeout -> kill"); a real server self-heals.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent listener");
    let addr = listener.local_addr().unwrap().to_string();

    let start = std::time::Instant::now();
    let v = probe_session_liveness(
        &addr,
        "key",
        Duration::from_millis(300),
        Duration::from_millis(150),
    );

    assert_eq!(v, SessionLiveness::Dead, "silent peer must be Dead after timeout");
    assert!(start.elapsed() < Duration::from_secs(2), "probe must stay bounded, not hang");
    drop(listener);
}

#[test]
fn registry_dir_is_sandboxed_under_cargo_test() {
    // The whole point of PSMUX_REGISTRY_DIR: `cargo test` must NEVER touch the
    // user's real ~/.psmux registry (a killed test run used to claim/orphan the
    // user's live warm server). Under cfg(test) the resolver defaults to a
    // per-process temp sandbox and exports it so spawned children inherit it.
    let dir = registry_dir();
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let real = format!("{}\\.psmux", home);
    assert!(!dir.eq_ignore_ascii_case(&real), "test registry must not be the real ~/.psmux");
    assert!(std::path::Path::new(dir).is_dir(), "registry dir is created on first resolution");
    let exported = std::env::var("PSMUX_REGISTRY_DIR").unwrap_or_default();
    assert_eq!(exported, dir, "resolved dir is exported for child processes");
}

#[test]
fn namespace_has_other_live_session_empty_registry_is_false() {
    // Sandbox registry starts empty for this namespace: killing the last
    // session in it must see "no other live session".
    assert!(!namespace_has_other_live_session(Some("nsholstest_none"), "nsholstest_none__self"));
}

#[test]
fn retire_warm_server_is_a_noop_without_a_warm_entry() {
    // No warm registry entry for this namespace: nothing to contact, no panic,
    // and nothing new may appear in the registry.
    let dir = registry_dir();
    retire_warm_server(Some("rwtest_none"));
    assert!(!std::path::Path::new(&format!("{}\\rwtest_none____warm__.port", dir)).exists());
}

#[test]
fn retire_warm_server_swallows_garbage_and_dead_entries() {
    // Namespaced warm base so parallel tests scanning the shared sandbox
    // registry never mistake these fixtures for their own sessions.
    let dir = registry_dir();
    let warm_base = "rwtest_dead____warm__";
    let port_path = format!("{}\\{}.port", dir, warm_base);
    let key_path = format!("{}\\{}.key", dir, warm_base);

    // Garbage port file: parse fails -> silent no-op, but the retire WON the
    // rename, so the (useless) pointer is consumed.
    std::fs::write(&port_path, "not-a-port").unwrap();
    retire_warm_server(Some("rwtest_dead"));
    assert!(!std::path::Path::new(&port_path).exists(), "won rename must consume the pointer");

    // Well-formed entry pointing at a dead port: connect fails -> swallowed,
    // pointer likewise consumed (a dead warm entry must not linger).
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = l.local_addr().unwrap().port();
    drop(l);
    std::fs::write(&port_path, dead_port.to_string()).unwrap();
    std::fs::write(&key_path, "0123456789abcdef").unwrap();
    retire_warm_server(Some("rwtest_dead"));
    assert!(!std::path::Path::new(&port_path).exists(), "dead warm pointer must be consumed");

    let _ = std::fs::remove_file(&port_path);
    let _ = std::fs::remove_file(&key_path);
}

#[test]
fn retire_warm_server_loses_the_rename_to_a_claimant() {
    // Claim-vs-retire mutual exclusion: the claim path commits by atomically
    // renaming `__warm__.port` away. Once that rename happened, a concurrent
    // retire must find nothing to rename and walk away — it must NOT contact
    // (and kill) the warm server the claimant now owns.
    let dir = registry_dir();
    let warm_base = "rwtest_claimed____warm__";
    let port_path = format!("{}\\{}.port", dir, warm_base);

    // A live listener stands in for the warm server mid-claim: if retire
    // (incorrectly) contacted it, the connect would succeed. The pointer
    // has already been renamed away by the "claimant".
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let live_port = l.local_addr().unwrap().port();
    let claimed_path = format!("{}\\{}.port.claiming", dir, warm_base);
    std::fs::write(&claimed_path, live_port.to_string()).unwrap(); // claimant's handoff
    assert!(!std::path::Path::new(&port_path).exists());

    retire_warm_server(Some("rwtest_claimed"));

    // Nothing connected to the listener (no pending accept) and the
    // claimant's handoff file is untouched.
    l.set_nonblocking(true).unwrap();
    assert!(l.accept().is_err(), "retire must not contact a warm owned by a claimant");
    assert!(std::path::Path::new(&claimed_path).exists(), "claimant handoff must be untouched");
    let _ = std::fs::remove_file(&claimed_path);
}

#[test]
fn namespace_has_other_live_session_dead_entry_is_false_unverifiable_is_true() {
    let ns = "nsholstest";
    let dir = registry_dir();

    // A .port pointing at a dead loopback port with a well-formed key file:
    // probe classifies Dead -> not "present".
    let dead_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead_listener.local_addr().unwrap().port();
    drop(dead_listener); // port now refuses connections
    let dead_base = format!("{}__deadone", ns);
    std::fs::write(format!("{}\\{}.port", dir, dead_base), dead_port.to_string()).unwrap();
    std::fs::write(format!("{}\\{}.key", dir, dead_base), "0123456789abcdef").unwrap();
    assert!(
        !namespace_has_other_live_session(Some(ns), &format!("{}__self", ns)),
        "a definitively dead entry must not count as a live session"
    );

    // No .key at all: identity can't be verified -> Unreachable -> conservative
    // "present" (kill-session must NOT retire the warm standby on uncertainty).
    let unver_base = format!("{}__unverifiable", ns);
    std::fs::write(format!("{}\\{}.port", dir, unver_base), dead_port.to_string()).unwrap();
    assert!(
        namespace_has_other_live_session(Some(ns), &format!("{}__self", ns)),
        "an unverifiable entry must conservatively count as present"
    );

    // excluded base is ignored even when its files exist
    let _ = std::fs::remove_file(format!("{}\\{}.port", dir, unver_base));
    let self_base = format!("{}__self", ns);
    std::fs::write(format!("{}\\{}.port", dir, self_base), dead_port.to_string()).unwrap();
    assert!(!namespace_has_other_live_session(Some(ns), &self_base), "own base must be excluded");

    for b in [&dead_base, &unver_base, &self_base] {
        let _ = std::fs::remove_file(format!("{}\\{}.port", dir, b));
        let _ = std::fs::remove_file(format!("{}\\{}.key", dir, b));
    }
}
