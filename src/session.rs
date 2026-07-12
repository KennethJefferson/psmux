use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};
use std::env;

const STALE_PORT_PROBE_ATTEMPTS: usize = 3;
const STALE_PORT_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
const STALE_PORT_RETRY_DELAY: Duration = Duration::from_millis(25);
/// How long to wait for the server's AUTH ack (`OK` / `ERROR`) when verifying
/// that the listener on a port file's port is actually *our* psmux server.
const STALE_PORT_AUTH_READ_TIMEOUT: Duration = Duration::from_millis(120);
/// Grace window subtracted from the system boot time before treating a
/// registry file as "written before this boot". Absorbs clock jitter and the
/// inherent imprecision of deriving boot wall-time from uptime, so a server
/// that wrote its port file moments after boot is never falsely reaped.
const BOOT_TIME_MARGIN: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PortProbeResult {
    Alive,
    Stale,
    Inconclusive,
}

/// Returns true if this port-file base name belongs to a warm (standby) server.
/// Warm sessions should be hidden from user-facing lists and never auto-attached.
pub fn is_warm_session(base: &str) -> bool {
    base == "__warm__" || base.ends_with("____warm__")
}

/// Find the next available numeric session name (tmux-compatible).
/// tmux uses a monotonically incrementing counter, but since psmux has
/// no persistent server state, we scan existing port files and pick
/// the lowest non-negative integer not already in use.
/// When `ns_prefix` is Some("foo"), names are checked as "foo__0", "foo__1", etc.
pub fn next_session_name(ns_prefix: Option<&str>) -> String {
    let home = match env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
        Ok(h) => h,
        Err(_) => return "0".to_string(),
    };
    let psmux_dir = format!("{}\\.psmux", home);
    let mut used: std::collections::HashSet<u32> = std::collections::HashSet::new();
    if let Ok(entries) = std::fs::read_dir(&psmux_dir) {
        for entry in entries.flatten() {
            if let Some(fname) = entry.file_name().to_str() {
                if let Some((base, ext)) = fname.rsplit_once('.') {
                    if ext != "port" { continue; }
                    if is_warm_session(base) { continue; }
                    // Extract the session name part (after namespace prefix if any)
                    let session_part = if let Some(pfx) = ns_prefix {
                        let full_pfx = format!("{}__", pfx);
                        if base.starts_with(&full_pfx) {
                            &base[full_pfx.len()..]
                        } else {
                            continue; // different namespace
                        }
                    } else {
                        if base.contains("__") { continue; } // namespaced session
                        base
                    };
                    if let Ok(n) = session_part.parse::<u32>() {
                        used.insert(n);
                    }
                }
            }
        }
    }
    let mut id = 0u32;
    while used.contains(&id) {
        id += 1;
    }
    id.to_string()
}

/// Serializes session-id allocation within this process. Without it, two
/// threads (e.g. concurrent `new-session` handling, or the test harness running
/// tests in parallel) can both read the same value from the counter file before
/// either writes back, and hand out duplicate ids.
static SESSION_ID_ALLOC: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Best-effort cross-process advisory lock backed by an atomically-created lock
/// file. Separate psmux server processes share the same `next_session_id`
/// counter, so the in-process mutex alone is not enough; this closes the
/// read-modify-write gap across processes too. Released on drop. A lock left by
/// a crashed process is taken over once it is clearly stale; the guarded
/// critical section is sub-millisecond, so the staleness bound never steals a
/// live lock.
struct CounterLock {
    path: String,
}

impl CounterLock {
    const STALE_AFTER: Duration = Duration::from_secs(5);

    fn acquire(path: String) -> Self {
        for _ in 0..2000 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return CounterLock { path };
                }
                Err(_) => {
                    // Take over a stale lock left behind by a crashed holder.
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().map(|d| d >= Self::STALE_AFTER).unwrap_or(false))
                        .unwrap_or(false);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        // Never observed in practice (the critical section is microseconds);
        // proceed rather than hang session creation indefinitely.
        CounterLock { path }
    }
}

impl Drop for CounterLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Allocate a globally unique session ID by reading and incrementing
/// the persistent counter file `.psmux/next_session_id`.
///
/// The read-modify-write is serialized within the process by `SESSION_ID_ALLOC`
/// and across processes by an advisory lock file, so concurrent callers can
/// never observe the same `current` and return duplicate ids.
pub fn allocate_session_id() -> usize {
    let _guard = SESSION_ID_ALLOC.lock().unwrap_or_else(|e| e.into_inner());
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let counter_path = format!("{}\\.psmux\\next_session_id", home);
    let _xlock = CounterLock::acquire(format!("{}.lock", counter_path));
    let current = std::fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let _ = std::fs::write(&counter_path, (current + 1).to_string());
    current
}

/// Write a `.sid` file recording the session ID for this session.
pub fn write_session_id_file(port_file_base: &str, session_id: usize) {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let sid_path = format!("{}\\.psmux\\{}.sid", home, port_file_base);
    let _ = std::fs::write(&sid_path, session_id.to_string());
}

/// Remove the `.sid` file for a session. Also removes the twin `.pid` file
/// (issue #448): both are per-session identity sentinels written together by
/// `ensure_session_registry_files`, and every session-teardown site already
/// calls this, so piggybacking `.pid` cleanup here keeps the registry consistent
/// without touching each teardown call site.
pub fn remove_session_id_file(port_file_base: &str) {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let sid_path = format!("{}\\.psmux\\{}.sid", home, port_file_base);
    let _ = std::fs::remove_file(&sid_path);
    remove_session_pid_file(port_file_base);
}

/// Write a `.pid` file recording the OS process ID of the server that owns this
/// session (issue #448). The stale-port cleanup only knew a server by its TCP
/// port; a wedged server that stopped listening but hasn't exited could not be
/// targeted by identity at all. The PID gives every registry entry a stable
/// process anchor.
pub fn write_session_pid_file(port_file_base: &str, pid: u32) {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let pid_path = format!("{}\\.psmux\\{}.pid", home, port_file_base);
    let _ = std::fs::write(&pid_path, pid.to_string());
}

/// Remove the `.pid` file for a session.
pub fn remove_session_pid_file(port_file_base: &str) {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let pid_path = format!("{}\\.psmux\\{}.pid", home, port_file_base);
    let _ = std::fs::remove_file(&pid_path);
}

/// Resolve a tmux session ID (`$N`) to the port file base name of the
/// session that owns that ID. Returns `None` if no session has that ID.
pub fn resolve_session_by_id(id: usize) -> Option<String> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).ok()?;
    let psmux_dir = format!("{}\\.psmux", home);
    if let Ok(entries) = std::fs::read_dir(&psmux_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "sid").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(file_id) = content.trim().parse::<usize>() {
                        if file_id == id {
                            if let Some(base) = path.file_stem().and_then(|s| s.to_str()) {
                                // Verify the session is actually alive
                                let port_path = format!("{}\\.psmux\\{}.port", home, base);
                                if std::path::Path::new(&port_path).exists() {
                                    return Some(base.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Clean up any stale port files (where server is not actually running)
pub fn cleanup_stale_port_files() {
    let home = match env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
        Ok(h) => h,
        Err(_) => return,
    };
    let psmux_dir = format!("{}\\.psmux", home);
    cleanup_stale_port_files_in(Path::new(&psmux_dir));
}

fn cleanup_stale_port_files_in(psmux_dir: &Path) {
    cleanup_stale_port_files_in_with(psmux_dir, probe_session_for_cleanup);
}

/// Image-name stems (lower-case, no extension) that count as a psmux server for
/// the orphan reaper. Only processes whose executable matches one of these are
/// ever candidates for termination — an unrelated app that happens to hold a
/// loopback listener is never touched.
const PSMUX_SERVER_IMAGE_NAMES: &[&str] = &["psmux", "tmux", "pmux"];

/// Grace period before a live server process is eligible for orphan reaping.
/// A server that just bound its socket but hasn't finished writing its `.port`
/// file yet (or a concurrent `new-session` still coming up) would otherwise look
/// untracked; requiring the process to be older than this avoids that race. The
/// spawn-race itself is fixed in #444 — this reaper is only the accumulation
/// backstop, so it can afford to skip very young processes and catch them next
/// startup instead.
const ORPHAN_REAP_MIN_AGE: Duration = Duration::from_secs(10);

/// A live psmux server process discovered by the reaper: its PID, every loopback
/// port it listens on, and its process creation time (FILETIME 100ns ticks).
#[derive(Clone, Debug, PartialEq)]
struct ServerCandidate {
    pid: u32,
    ports: Vec<u16>,
    creation_ft: u64,
}

/// Pure orphan-selection policy (unit-testable, no OS calls).
///
/// A candidate server is an orphan to reap iff ALL hold:
///  - it is not this very process (`self_pid`),
///  - its PID is not recorded in any live registry entry (`tracked_pids`),
///  - NONE of its listening ports is claimed by a registry `.port` file
///    (`tracked_ports`) — i.e. nothing references this server, so it is a
///    duplicate / lost headless server rather than a legitimate session,
///  - it was created at or before `age_cutoff_ft` (older than the grace window),
///    so a just-spawned server still writing its registry files is never reaped.
///
/// The port check is the primary anchor: a legitimate server ALWAYS has a
/// `.port` file pointing at it, so it can never be selected even if its `.pid`
/// file is missing (backward compatibility with servers started before #448).
fn select_orphan_pids(
    candidates: &[ServerCandidate],
    tracked_ports: &std::collections::HashSet<u16>,
    tracked_pids: &std::collections::HashSet<u32>,
    self_pid: u32,
    age_cutoff_ft: u64,
) -> Vec<u32> {
    let mut out = Vec::new();
    for c in candidates {
        if c.pid == self_pid { continue; }
        if tracked_pids.contains(&c.pid) { continue; }
        if c.ports.iter().any(|p| tracked_ports.contains(p)) { continue; }
        // Only reap processes old enough to have finished registering.
        if age_cutoff_ft != 0 && c.creation_ft > age_cutoff_ft { continue; }
        out.push(c.pid);
    }
    out
}

/// Read the set of ports referenced by `.port` files and the set of PIDs
/// recorded in `.pid` files whose sibling `.port` still exists. A `.pid` without
/// a live `.port` is ignored so a dead-then-reused PID can't be treated as
/// tracked.
fn read_tracked_registry(psmux_dir: &Path)
    -> (std::collections::HashSet<u16>, std::collections::HashSet<u32>)
{
    let mut tracked_ports = std::collections::HashSet::new();
    let mut tracked_pids = std::collections::HashSet::new();
    if let Ok(entries) = std::fs::read_dir(psmux_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("port") => {
                    if let Ok(s) = std::fs::read_to_string(&path) {
                        if let Ok(p) = s.trim().parse::<u16>() { tracked_ports.insert(p); }
                    }
                }
                Some("pid") => {
                    // Only trust a PID whose session still has a live .port file.
                    let port_sibling = path.with_extension("port");
                    if port_sibling.exists() {
                        if let Ok(s) = std::fs::read_to_string(&path) {
                            if let Ok(pid) = s.trim().parse::<u32>() { tracked_pids.insert(pid); }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    (tracked_ports, tracked_pids)
}

/// Terminate live psmux *server* processes that no registry entry accounts for
/// (issue #448). Complements `cleanup_stale_port_files`, which only removes
/// registry files for servers already proven dead: this pass finds a live but
/// orphaned server (a spawn-race duplicate, or a crashed client's headless
/// server) and reaps the process itself, bounding the process count regardless
/// of how the duplicate arose.
pub fn reap_orphaned_servers() {
    let home = match env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
        Ok(h) => h,
        Err(_) => return,
    };
    let psmux_dir = format!("{}\\.psmux", home);
    reap_orphaned_servers_in(Path::new(&psmux_dir));
}

fn reap_orphaned_servers_in(psmux_dir: &Path) {
    use crate::platform::process_kill;

    let (tracked_ports, tracked_pids) = read_tracked_registry(psmux_dir);
    let self_pid = std::process::id();

    // Capture the reuse-guard cutoff BEFORE enumerating: any process we see now
    // was created at or before this instant, so a PID reused afterwards is
    // rejected by terminate_server_pid (#447 guard).
    let now_ft = process_kill::now_process_filetime();
    // 100ns ticks in the grace window; a process is "old enough" to reap only if
    // its creation time is at or before now - grace.
    let grace_ticks = (ORPHAN_REAP_MIN_AGE.as_nanos() / 100) as u64;
    let age_cutoff_ft = now_ft.saturating_sub(grace_ticks);

    // Group loopback listeners by PID, keeping only psmux-image server processes.
    let mut by_pid: std::collections::HashMap<u32, Vec<u16>> = std::collections::HashMap::new();
    for (pid, port) in process_kill::loopback_listener_pids() {
        by_pid.entry(pid).or_default().push(port);
    }
    let mut candidates: Vec<ServerCandidate> = Vec::new();
    for (pid, ports) in by_pid {
        let is_psmux = crate::platform::process_info::get_process_name(pid)
            .map(|n| {
                let n = n.to_ascii_lowercase();
                PSMUX_SERVER_IMAGE_NAMES.contains(&n.as_str())
            })
            .unwrap_or(false);
        if !is_psmux { continue; }
        let creation_ft = process_kill::process_creation_time(pid).unwrap_or(u64::MAX);
        candidates.push(ServerCandidate { pid, ports, creation_ft });
    }

    let orphans = select_orphan_pids(&candidates, &tracked_ports, &tracked_pids, self_pid, age_cutoff_ft);
    for pid in orphans {
        if crate::debug_log::session_log_enabled() {
            crate::debug_log::session_log("reaper", &format!(
                "terminating orphaned psmux server pid {} (no registry entry references it)", pid));
        }
        process_kill::terminate_server_pid(pid, now_ft);
    }
}

/// Windows FILETIME ticks (100ns since 1601-01-01) for a `SystemTime`.
/// Used to compare a process creation time against a registry file mtime.
fn system_time_to_filetime_ticks(t: SystemTime) -> Option<u64> {
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;
    let since_unix = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(UNIX_EPOCH_AS_FILETIME.saturating_add((since_unix.as_nanos() / 100) as u64))
}

/// Slack allowed between a `.pid` file's last write and the recorded process's
/// creation time before the PID is considered recycled. A legitimate server is
/// always created BEFORE it writes its `.pid`, so its creation time can never
/// exceed the file mtime; the margin only absorbs filesystem/clock jitter.
const PID_REUSE_MARGIN_TICKS: u64 = 60 * 10_000_000; // 60s in 100ns ticks

/// Definitive liveness verdict from the `.pid` sentinel written next to every
/// `.port` file (issue #448) — no network round-trip.
///
/// This is what keeps CLI startup O(microseconds) per registry entry: a TCP
/// probe of a dead port can burn its full connect timeout on Windows (stealth
/// firewall behavior never sends RST on loopback for some configurations) and
/// then classify as Inconclusive, leaving the stale file to tax EVERY future
/// invocation. The process table answers instantly and definitively.
///
/// Returns:
///   Some(true)  - recorded PID is a live psmux-image process created no later
///                 than the `.pid` file was written -> genuinely our server.
///   Some(false) - PID gone, recycled by a non-psmux image, or recycled by a
///                 psmux process created long after the file -> server is dead.
///   None        - no usable `.pid` anchor (pre-#448 registry) -> caller must
///                 fall back to the network probe.
fn pid_anchor_verdict(port_path: &Path) -> Option<bool> {
    // The process-table queries below are Windows-only; other platforms fall
    // back to the network probe rather than misreading stub returns as "dead".
    if !cfg!(windows) {
        return None;
    }
    let pid_path = port_path.with_extension("pid");
    let pid: u32 = std::fs::read_to_string(&pid_path).ok()?.trim().parse().ok()?;
    let name = match crate::platform::process_info::get_process_name(pid) {
        // No such process (a same-user psmux server is always openable with
        // QUERY_LIMITED_INFORMATION, so an unopenable PID is not our server).
        None => return Some(false),
        Some(n) => n.to_ascii_lowercase(),
    };
    if !PSMUX_SERVER_IMAGE_NAMES.contains(&name.as_str()) {
        // PID recycled by an unrelated application; our server is gone.
        return Some(false);
    }
    // PID-reuse guard (same idea as the #447 reaper guard): a psmux process
    // created well AFTER the .pid file was last written cannot be the server
    // that wrote it. When either timestamp is unavailable, err towards alive.
    if let Some(created_ft) = crate::platform::process_kill::process_creation_time(pid) {
        if let Some(mtime_ft) = std::fs::metadata(&pid_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(system_time_to_filetime_ticks)
        {
            if created_ft > mtime_ft.saturating_add(PID_REUSE_MARGIN_TICKS) {
                return Some(false);
            }
        }
    }
    Some(true)
}

/// Resolve the session key stored alongside a `.port` file (the sibling
/// `.key`). Returns an empty string when the key file is missing, which the
/// identity probe treats as "cannot verify" (Inconclusive) rather than dead.
fn read_key_for_port_path(port_path: &Path) -> String {
    let key_path = port_path.with_extension("key");
    std::fs::read_to_string(&key_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Best-effort wall-clock time the system last booted, derived from uptime.
///
/// Any registry file last written before this instant cannot belong to a
/// server that has been running since the machine started — its owning
/// process died with the previous boot (e.g. an OS-update reboot). This is
/// the reliable signal for cleaning up sessions orphaned by a restart, and
/// it does not depend on the network (the old port may now be free, occupied
/// by an unrelated process, or even reused by a *different* live psmux
/// server — all of which a bare TCP probe would misclassify).
#[cfg(windows)]
fn system_boot_time() -> Option<SystemTime> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTickCount64() -> u64;
    }
    let uptime_ms = unsafe { GetTickCount64() };
    SystemTime::now().checked_sub(Duration::from_millis(uptime_ms))
}

#[cfg(not(windows))]
fn system_boot_time() -> Option<SystemTime> {
    None
}

/// True when `mtime` is old enough (older than `boot - margin`) that the file
/// must have been written by a process from a previous boot.
fn is_pre_boot(mtime: SystemTime, boot: SystemTime, margin: Duration) -> bool {
    match boot.checked_sub(margin) {
        Some(cutoff) => mtime < cutoff,
        None => false,
    }
}

fn cleanup_stale_port_files_in_with<F>(psmux_dir: &Path, mut probe: F)
where
    F: FnMut(&str, u16) -> PortProbeResult,
{
    let boot = system_boot_time();
    if let Ok(entries) = std::fs::read_dir(psmux_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "port").unwrap_or(false) {
                // Boot-time guard: a port file last modified before this boot
                // belongs to a server that died when the machine restarted.
                // Reap it unconditionally — no network round-trip, and immune
                // to the old port being reused by another process this boot.
                if let Some(boot) = boot {
                    if let Some(mtime) = entry.metadata().ok().and_then(|m| m.modified().ok()) {
                        if is_pre_boot(mtime, boot, BOOT_TIME_MARGIN) {
                            if crate::debug_log::session_log_enabled() {
                                crate::debug_log::session_log("cleanup", &format!(
                                    "reaping '{}': port file predates last boot (server died on restart)",
                                    registry_base(&path)));
                            }
                            remove_session_registry_files(&path);
                            continue;
                        }
                    }
                }
                // PID-anchor fast path (issue #448 sentinel): the process table
                // answers liveness instantly and definitively, so registry
                // entries with a `.pid` sibling never pay a network probe.
                // Dead-port probes are not just slow (they can burn the full
                // connect timeout per attempt on Windows loopback) - they are
                // also inconclusive, so stale files were never reaped and the
                // probe tax repeated on every subsequent CLI invocation.
                match pid_anchor_verdict(&path) {
                    Some(true) => continue, // live server; nothing to clean
                    Some(false) => {
                        if crate::debug_log::session_log_enabled() {
                            crate::debug_log::session_log("cleanup", &format!(
                                "reaping '{}': recorded server PID is dead or recycled",
                                registry_base(&path)));
                        }
                        remove_session_registry_files(&path);
                        continue;
                    }
                    None => {} // no .pid anchor; fall through to the network probe
                }
                if let Ok(port_str) = std::fs::read_to_string(&path) {
                    if let Ok(port) = port_str.trim().parse::<u16>() {
                        let key = read_key_for_port_path(&path);
                        if probe(&key, port) == PortProbeResult::Stale {
                            if crate::debug_log::session_log_enabled() {
                                crate::debug_log::session_log("cleanup", &format!(
                                    "reaping '{}' (port {}): no psmux server authenticated as ours (stale)",
                                    registry_base(&path), port));
                            }
                            remove_session_registry_files(&path);
                        }
                    } else {
                        if crate::debug_log::session_log_enabled() {
                            crate::debug_log::session_log("cleanup", &format!(
                                "reaping '{}': unparseable port value {:?}",
                                registry_base(&path), port_str.trim()));
                        }
                        remove_session_registry_files(&path);
                    }
                }
            }
        }
    }
}

/// Display name (file stem) of a registry path, for logging.
fn registry_base(port_path: &Path) -> &str {
    port_path.file_stem().and_then(|s| s.to_str()).unwrap_or("?")
}

fn remove_session_registry_files(port_path: &Path) {
    let _ = std::fs::remove_file(port_path);
    let key_path = port_path.with_extension("key");
    let _ = std::fs::remove_file(&key_path);
    let sid_path = port_path.with_extension("sid");
    let _ = std::fs::remove_file(&sid_path);
    // Also drop the twin .pid sentinel (issue #448) so a dead server's PID
    // never lingers to be mistaken for a live tracked process by the reaper.
    let pid_path = port_path.with_extension("pid");
    let _ = std::fs::remove_file(&pid_path);
}

/// Outcome of a single AUTH handshake against the listener on a port.
#[derive(Clone, Copy, PartialEq)]
enum AuthProbe {
    /// Server accepted our session key (`OK`) — this is genuinely our session.
    Authenticated,
    /// Server explicitly rejected the key (`ERROR ...`) — a *different* psmux
    /// server has reused this port; the session this file names is dead.
    Rejected,
    /// Connected but the peer didn't complete our protocol (no reply, garbage,
    /// or an unrelated process). Identity is unverifiable from the network.
    Unknown,
}

/// Connect to `addr` and verify, via the AUTH handshake, that the listener is
/// the psmux server that owns `key`.
///
/// Returns `Err(kind)` when the connection itself fails so the caller can tell
/// "nothing is listening" (refused) from a transient network error.
fn probe_auth_identity(addr: std::net::SocketAddr, key: &str) -> Result<AuthProbe, ErrorKind> {
    let mut s = std::net::TcpStream::connect_timeout(&addr, STALE_PORT_CONNECT_TIMEOUT)
        .map_err(|e| e.kind())?;
    // A successful connect alone proves only that *something* listens. Without
    // a key we cannot prove it is ours, so leave the verdict to the boot guard.
    let key = match validate_auth_key(key) {
        Some(k) => k,
        None => return Ok(AuthProbe::Unknown),
    };
    let _ = s.set_read_timeout(Some(STALE_PORT_AUTH_READ_TIMEOUT));
    let _ = s.set_nodelay(true);
    if write!(s, "AUTH {}\n", key).is_err() {
        return Ok(AuthProbe::Unknown);
    }
    let _ = s.flush();
    let mut br = std::io::BufReader::new(std::io::Read::take(s, 4096));
    let mut line = String::new();
    match std::io::BufRead::read_line(&mut br, &mut line) {
        Ok(0) => Ok(AuthProbe::Unknown),
        Ok(_) => {
            let t = line.trim();
            if t == "OK" {
                Ok(AuthProbe::Authenticated)
            } else if t.starts_with("ERROR") {
                Ok(AuthProbe::Rejected)
            } else {
                Ok(AuthProbe::Unknown)
            }
        }
        Err(_) => Ok(AuthProbe::Unknown),
    }
}

/// Identity-aware liveness probe used by stale-port cleanup.
///
/// A bare TCP connect cannot distinguish our server from any other process
/// that grabbed the same port after a crash or reboot — that false "alive"
/// is exactly what left dead sessions showing `(not responding)` in the
/// picker. This probe instead requires the AUTH key to match:
///   - connection refused on every attempt        -> `Stale`
///   - server accepts our key (`OK`)              -> `Alive`
///   - server rejects our key (`ERROR`, reused port) -> `Stale`
///   - anything ambiguous (no reply, slow, foreign process) -> `Inconclusive`
///
/// Only definitive signals delete a file; ambiguous ones are left for the
/// boot-time guard, so a live-but-busy server is never reaped by mistake.
fn probe_session_for_cleanup(key: &str, port: u16) -> PortProbeResult {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut saw_refused = false;
    let mut saw_inconclusive = false;

    for attempt in 0..STALE_PORT_PROBE_ATTEMPTS {
        match probe_auth_identity(addr, key) {
            Ok(AuthProbe::Authenticated) => {
                if crate::debug_log::session_log_enabled() {
                    crate::debug_log::session_log("probe",
                        &format!("port {}: AUTH accepted -> alive", port));
                }
                return PortProbeResult::Alive;
            }
            Ok(AuthProbe::Rejected) => {
                if crate::debug_log::session_log_enabled() {
                    crate::debug_log::session_log("probe", &format!(
                        "port {}: AUTH rejected by a different server (reused port) -> stale", port));
                }
                return PortProbeResult::Stale;
            }
            Ok(AuthProbe::Unknown) => saw_inconclusive = true,
            Err(ErrorKind::ConnectionRefused) => saw_refused = true,
            Err(_) => saw_inconclusive = true,
        }

        if attempt + 1 < STALE_PORT_PROBE_ATTEMPTS {
            std::thread::sleep(STALE_PORT_RETRY_DELAY);
        }
    }

    if saw_refused && !saw_inconclusive {
        if crate::debug_log::session_log_enabled() {
            crate::debug_log::session_log("probe",
                &format!("port {}: connection refused on all attempts -> stale", port));
        }
        PortProbeResult::Stale
    } else {
        if crate::debug_log::session_log_enabled() {
            crate::debug_log::session_log("probe",
                &format!("port {}: no definitive answer -> inconclusive (kept)", port));
        }
        PortProbeResult::Inconclusive
    }
}

/// Read the session key from the key file
pub fn read_session_key(session: &str) -> io::Result<String> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let keypath = format!("{}\\.psmux\\{}.key", home, session);
    std::fs::read_to_string(&keypath).map(|s| s.trim().to_string())
}

/// Hard cap on a single response payload read from the server (256 KB).
///
/// The server is trusted, but the client should still bound how much memory
/// a single picker fetch can consume. A buggy or malicious peer that sends
/// an unbounded line with no `\n` would otherwise block until the read
/// timeout while filling the BufReader. 256 KB is comfortably larger than
/// any real `session-info`, `list-tree`, or `choose-buffer` payload.
pub const MAX_AUTHED_RESPONSE_BYTES: u64 = 256 * 1024;

/// Validate that a session key is well-formed for the line-oriented AUTH
/// protocol. Rejects keys containing CR, LF, or NUL — anything that could
/// terminate the AUTH line early or smuggle a second protocol frame.
///
/// Returns the trimmed key on success, `None` on rejection.
///
/// SECURITY: Without this check, a key sourced from a future caller (e.g.
/// env var, IPC, plugin) that contains `\n` could inject a second command
/// onto the AUTH line. All AUTH writers should funnel through this guard.
pub fn validate_auth_key(key: &str) -> Option<&str> {
    let k = key.trim_matches(|c: char| c == '\r' || c == '\n');
    if k.is_empty() {
        return None;
    }
    if k.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return None;
    }
    Some(k)
}

/// Send an authenticated command to a server (fire-and-forget).
///
/// Validates the key against CRLF/NUL injection. Silently no-ops on a
/// malformed key — callers are at the trust boundary already (key file
/// under user's profile), this is defense-in-depth.
pub fn send_auth_cmd(addr: &str, key: &str, cmd: &[u8]) -> io::Result<()> {
    let key = match validate_auth_key(key) {
        Some(k) => k,
        None => return Ok(()),
    };
    let sock_addr: std::net::SocketAddr = addr.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if let Ok(mut s) = std::net::TcpStream::connect_timeout(&sock_addr, Duration::from_millis(50)) {
        let _ = s.set_nodelay(true);
        let _ = write!(s, "AUTH {}\n", key);
        let _ = std::io::Write::write_all(&mut s, cmd);
        let _ = s.flush();
    }
    Ok(())
}

/// Send an authenticated command and get response.
///
/// Validates the key, caps the response at `MAX_AUTHED_RESPONSE_BYTES`,
/// and returns whatever the server sent after the AUTH ack. The `OK\n`
/// ack is **not** stripped here for backward compatibility with existing
/// callers; new code should prefer `fetch_authed_response` /
/// `fetch_authed_response_multi`.
pub fn send_auth_cmd_response(addr: &str, key: &str, cmd: &[u8]) -> io::Result<String> {
    let key = match validate_auth_key(key) {
        Some(k) => k,
        None => return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid session key")),
    };
    let mut s = std::net::TcpStream::connect(addr)?;
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = write!(s, "AUTH {}\n", key);
    let _ = std::io::Write::write_all(&mut s, cmd);
    let _ = s.flush();
    let mut br = std::io::BufReader::new(std::io::Read::take(&mut s, MAX_AUTHED_RESPONSE_BYTES));
    let mut auth_line = String::new();
    let _ = std::io::BufRead::read_line(&mut br, &mut auth_line);
    let mut buf = String::new();
    let _ = std::io::Read::read_to_string(&mut br, &mut buf);
    Ok(buf)
}

/// Internal: open an authenticated connection and send a single command.
///
/// Returns a length-capped `BufReader` positioned right after the command
/// write, ready for response parsing. Centralizes:
///   - CRLF/NUL key validation (security)
///   - connect timeout, read timeout, TCP_NODELAY
///   - response size cap (`MAX_AUTHED_RESPONSE_BYTES`, DoS guard)
///   - the AUTH + command write
///
/// The size cap is applied with `Read::take` BEFORE the `BufReader` so the
/// resulting reader still exposes `BufRead`. Wrapping the other way around
/// (`BufReader::take`) loses `BufRead` because `Take` is `Read`-only.
fn open_authed(
    addr: &str,
    key: &str,
    cmd: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Option<std::io::BufReader<std::io::Take<std::net::TcpStream>>> {
    let key = validate_auth_key(key)?;
    let sock_addr: std::net::SocketAddr = addr.parse().ok()?;
    let mut s = std::net::TcpStream::connect_timeout(&sock_addr, connect_timeout).ok()?;
    s.set_read_timeout(Some(read_timeout)).ok()?;
    let _ = s.set_nodelay(true);
    write!(s, "AUTH {}\n", key).ok()?;
    s.write_all(cmd).ok()?;
    if !cmd.ends_with(b"\n") {
        s.write_all(b"\n").ok()?;
    }
    let _ = s.flush();
    Some(std::io::BufReader::new(std::io::Read::take(s, MAX_AUTHED_RESPONSE_BYTES)))
}

/// Read one response line from an authenticated stream, transparently
/// skipping the `OK\n` AUTH ack regardless of when it arrives.
///
/// Returns `None` on timeout, EOF, empty payload, or `ERROR:` reply.
/// Returns `Some(line)` on a valid payload (newline trimmed).
fn read_authed_line<R: std::io::BufRead>(br: &mut R) -> Option<String> {
    // First read: could be either the AUTH ack ("OK") or the payload
    // (if the ack was already pipelined into the same packet).
    let mut line = String::new();
    if std::io::BufRead::read_line(br, &mut line).ok()? == 0 {
        return None;
    }
    let trimmed = line.trim();
    if trimmed == "OK" {
        // First line WAS the ack. Read the real payload now.
        line.clear();
        if std::io::BufRead::read_line(br, &mut line).ok()? == 0 {
            return None;
        }
    }
    // Filter again in case the second line is also empty/error/OK.
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed == "OK" || trimmed.starts_with("ERROR:") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Read all remaining bytes from an authenticated stream, stripping a
/// leading `OK\n` AUTH ack if present.
///
/// Returns `None` on no payload, error response, or read failure.
/// Returns `Some(payload)` with the AUTH ack removed and trailing
/// whitespace stripped. Total read is capped by the underlying `Take`.
fn read_authed_all<R: std::io::Read>(rd: &mut R) -> Option<String> {
    let mut buf = String::new();
    std::io::Read::read_to_string(rd, &mut buf).ok()?;
    let body = buf.strip_prefix("OK\n").or_else(|| buf.strip_prefix("OK\r\n")).unwrap_or(&buf);
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed.starts_with("ERROR:") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Send an authenticated single-command request and return one response line.
///
/// Centralized AUTH + command + response helper used by all picker fetches.
/// Handles every known framing race for the AUTH ack:
///   - ack pipelined with payload (one packet, both lines arrive together)
///   - ack arrives first, then payload
///   - ack delayed past first read (issue #250 race)
///   - server replies only `OK` and never sends payload
///   - server replies `ERROR: ...`
///   - server hangs / connection refused / bad address
///
/// All callers get the same robust behavior; they can no longer reinvent
/// the parser per-site (which is how #250 happened).
pub fn fetch_authed_response(
    addr: &str,
    key: &str,
    cmd: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Option<String> {
    let mut br = open_authed(addr, key, cmd, connect_timeout, read_timeout)?;
    read_authed_line(&mut br)
}

/// Like `fetch_authed_response` but returns the entire response body
/// (multi-line payloads such as `list-tree` JSON arrays or `choose-buffer`
/// listings). The leading AUTH ack line is stripped if present.
///
/// The total payload is bounded by `MAX_AUTHED_RESPONSE_BYTES` to prevent
/// a malformed or hostile server from forcing unbounded client memory.
pub fn fetch_authed_response_multi(
    addr: &str,
    key: &str,
    cmd: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Option<String> {
    let mut br = open_authed(addr, key, cmd, connect_timeout, read_timeout)?;
    read_authed_all(&mut br)
}

/// Fetch a one-line `session-info` response from a session server.
///
/// Thin wrapper over `fetch_authed_response` retained for the call site
/// in `client.rs` (and the regression tests added in PR #251 for #250).
pub fn fetch_session_info(
    addr: &str,
    key: &str,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Option<String> {
    fetch_authed_response(addr, key, b"session-info\n", connect_timeout, read_timeout)
}

/// Fan out `fetch_session_info` across many sessions in parallel.
///
/// The session picker used to call `fetch_session_info` sequentially, so
/// opening the picker with N sessions was bounded by `N * read_timeout`
/// in the worst case. With this helper, N concurrent threads share that
/// bound: total wall time is roughly `read_timeout`, regardless of N.
///
/// `inputs` is `(label, addr, key)`. Output preserves input order and
/// pairs each label with the fetched info or the supplied `fallback`
/// (typically `"<label>: (not responding)"`).
///
/// Retained for the #250 regression suite; the picker now uses
/// `classify_sessions_parallel`, which both lists and prunes in one pass.
#[allow(dead_code)]
pub fn fetch_session_infos_parallel<F>(
    inputs: Vec<(String, String, String)>,
    connect_timeout: Duration,
    read_timeout: Duration,
    fallback: F,
) -> Vec<(String, String)>
where
    F: Fn(&str) -> String + Send + Sync,
{
    if inputs.is_empty() {
        return Vec::new();
    }
    // Single session: skip thread spawn overhead entirely.
    if inputs.len() == 1 {
        let (label, addr, key) = &inputs[0];
        let info = fetch_session_info(addr, key, connect_timeout, read_timeout)
            .unwrap_or_else(|| fallback(label));
        return vec![(label.clone(), info)];
    }
    let results: Vec<(String, String)> = std::thread::scope(|scope| {
        let fallback_ref = &fallback;
        let handles: Vec<_> = inputs
            .iter()
            .map(|(label, addr, key)| {
                let label = label.clone();
                let addr = addr.clone();
                let key = key.clone();
                scope.spawn(move || {
                    let info = fetch_session_info(&addr, &key, connect_timeout, read_timeout)
                        .unwrap_or_else(|| fallback_ref(&label));
                    (label, info)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    results
}

/// Liveness verdict for one session, produced by a single bounded probe.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionLiveness {
    /// Server authenticated and returned its session-info line (the payload).
    Alive(String),
    /// Definitively gone: the connection failed (refused / unreachable — on
    /// loopback any connect failure means nothing is listening), an `ERROR`
    /// auth rejection (a different server reused the port), or connected then
    /// silent past the read timeout. Its registry files should be reaped. A
    /// genuinely live server that was momentarily too slow self-heals: it
    /// rewrites its `.port`/`.key`/`.sid` every 5s (see
    /// `ensure_session_registry_files`).
    Dead,
    /// No usable AUTH key on disk, so identity cannot be verified at all.
    /// Left in place and shown as `(not responding)` rather than deleted.
    Unreachable,
}

/// Single bounded liveness probe: connect, AUTH with the session's own key,
/// ask for `session-info`, and classify the reply. Never retries or blocks
/// beyond `connect_timeout + read_timeout`.
fn probe_session_liveness(
    addr: &str,
    key: &str,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> SessionLiveness {
    let sock: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(_) => return SessionLiveness::Dead,
    };
    let key = match validate_auth_key(key) {
        Some(k) => k,
        None => return SessionLiveness::Unreachable,
    };
    // On loopback a live server always completes the TCP handshake (the kernel
    // accepts into the listen backlog even before the app calls accept()), so
    // ANY connect failure means nothing usable is listening -> Dead. We do not
    // branch on the error kind: Windows does not always surface a clean
    // `ConnectionRefused` for a free port.
    let mut s = match std::net::TcpStream::connect_timeout(&sock, connect_timeout) {
        Ok(s) => s,
        Err(_) => return SessionLiveness::Dead,
    };
    let _ = s.set_read_timeout(Some(read_timeout));
    let _ = s.set_nodelay(true);
    if write!(s, "AUTH {}\n", key).is_err() || s.write_all(b"session-info\n").is_err() {
        // Connection broke right after connect -> not a healthy server.
        return SessionLiveness::Dead;
    }
    let _ = s.flush();
    let mut br = std::io::BufReader::new(std::io::Read::take(s, MAX_AUTHED_RESPONSE_BYTES));
    let mut line = String::new();
    match std::io::BufRead::read_line(&mut br, &mut line) {
        Ok(0) => SessionLiveness::Dead,
        Ok(_) => {
            let t = line.trim();
            if t.starts_with("ERROR") {
                return SessionLiveness::Dead;
            }
            if t == "OK" {
                // Ack consumed; the next line is the real payload.
                line.clear();
                match std::io::BufRead::read_line(&mut br, &mut line) {
                    Ok(0) => SessionLiveness::Dead,
                    Ok(_) => {
                        let t2 = line.trim();
                        if t2.is_empty() || t2 == "OK" || t2.starts_with("ERROR") {
                            SessionLiveness::Dead
                        } else {
                            SessionLiveness::Alive(t2.to_string())
                        }
                    }
                    Err(_) => SessionLiveness::Dead,
                }
            } else {
                // Ack pipelined with the payload in one line.
                SessionLiveness::Alive(t.to_string())
            }
        }
        Err(_) => SessionLiveness::Dead,
    }
}

/// Classify many sessions in parallel with a single bounded probe each.
///
/// Like `fetch_session_infos_parallel`, total wall time is ~one probe window
/// regardless of N (each session runs on its own thread). Returns the liveness
/// verdict per input label, preserving order, so the caller can reap the dead
/// ones and render the rest. This is what keeps the session picker responsive:
/// it replaces a sequential cleanup pass (O(N * timeout)) with one parallel
/// round-trip that both lists and prunes.
pub fn classify_sessions_parallel(
    inputs: Vec<(String, String, String)>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Vec<(String, SessionLiveness)> {
    if inputs.is_empty() {
        return Vec::new();
    }
    if inputs.len() == 1 {
        let (label, addr, key) = &inputs[0];
        let v = probe_session_liveness(addr, key, connect_timeout, read_timeout);
        return vec![(label.clone(), v)];
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = inputs
            .iter()
            .map(|(label, addr, key)| {
                let label = label.clone();
                let addr = addr.clone();
                let key = key.clone();
                scope.spawn(move || {
                    let v = probe_session_liveness(&addr, &key, connect_timeout, read_timeout);
                    (label, v)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// PID-anchor liveness for the session registered under `base`, for
/// enumeration paths (e.g. CLI `list-sessions`) that would otherwise pay a
/// TCP connect timeout per dead entry. Some(false) = definitively dead
/// (reap + skip), Some(true) = live, None = no anchor (probe as usual).
pub fn registry_pid_anchor_alive(base: &str) -> Option<bool> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).ok()?;
    let port_path = format!("{}\\.psmux\\{}.port", home, base);
    pid_anchor_verdict(Path::new(&port_path))
}

/// Reap a single session's registry files (`.port`/`.key`/`.sid`) by base name.
///
/// Used when a probe proves the session is dead. Safe against a live server:
/// it re-creates these files on its next 5s registry tick.
pub fn remove_session_registry(base: &str) {
    let home = match env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
        Ok(h) => h,
        Err(_) => return,
    };
    let port_path = format!("{}\\.psmux\\{}.port", home, base);
    remove_session_registry_files(Path::new(&port_path));
}

pub fn send_control(line: String) -> io::Result<()> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let mut target = env::var("PSMUX_TARGET_SESSION").ok().unwrap_or_else(|| "default".to_string());
    // Never target a warm (standby) session — resolve to a real session instead
    if is_warm_session(&target) {
        // Extract namespace from warm session name (e.g. "foo____warm__" -> Some("foo"))
        let ns = target.strip_suffix("____warm__").map(|s| s.to_string());
        target = resolve_last_session_name_ns(ns.as_deref()).unwrap_or_else(|| "default".to_string());
    }
    let full_target = env::var("PSMUX_TARGET_FULL").ok();
    let path = format!("{}\\.psmux\\{}.port", home, target);
    let port = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u16>().ok()).ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("no server running on session '{}'", target)))?.clone();
    let session_key = read_session_key(&target).unwrap_or_default();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    // 1s connect timeout: a busy-but-alive server must not be mistaken for a
    // dead one. The old 100ms falsely tripped callers' stale-port cleanup,
    // deleting the port file of a server that was merely slow to accept.
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(1000))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(2000)));
    let _ = write!(stream, "AUTH {}\n", session_key);
    if let Some(ref ft) = full_target {
        let _ = write!(stream, "TARGET {}\n", ft);
    }
    let _ = write!(stream, "{}", line);
    let _ = stream.flush();
    // Half-close the write side so the server observes EOF *after* our bytes.
    // TCP guarantees all sent data is delivered before the FIN, so the server's
    // read_line always sees the full command before its loop ends — eliminating
    // the RST-on-close race that used to silently drop fire-and-forget commands
    // (the old 50ms "drain" read was only a partial mitigation).
    let _ = stream.shutdown(std::net::Shutdown::Write);
    // Drain to EOF (bounded by the read timeout): confirms the server stayed up
    // and consumed the command before we drop the socket.
    let mut buf = [0u8; 256];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

pub fn send_control_with_response(line: String) -> io::Result<String> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let mut target = env::var("PSMUX_TARGET_SESSION").ok().unwrap_or_else(|| "default".to_string());
    // Never target a warm (standby) session — resolve to a real session instead
    if is_warm_session(&target) {
        let ns = target.strip_suffix("____warm__").map(|s| s.to_string());
        target = resolve_last_session_name_ns(ns.as_deref()).unwrap_or_else(|| "default".to_string());
    }
    let full_target = env::var("PSMUX_TARGET_FULL").ok();
    let path = format!("{}\\.psmux\\{}.port", home, target);
    let port = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u16>().ok()).ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("no server running on session '{}'", target)))?.clone();
    let session_key = read_session_key(&target).unwrap_or_default();
    // Bounded connect: against a saturated listen backlog, a bare connect()
    // fails only after the ~21s Windows SYN-retransmit and surfaces as the
    // notorious `os error 10060`. A 1s timeout turns that into a fast, retryable
    // error instead of a multi-second hang printed to the user.
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "bad server address"))?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(1000))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(3000)));
    let _ = write!(stream, "AUTH {}\n", session_key);
    if let Some(ref ft) = full_target {
        let _ = write!(stream, "TARGET {}\n", ft);
    }
    let _ = write!(stream, "{}", line);
    let _ = stream.flush();
    // Half-close so the server sees EOF after our request and closes the socket
    // once the reply is complete — giving a definitive Ok(0) end-of-response
    // instead of relying on an idle-gap timeout to guess the reply is done.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = Vec::new();
    let mut temp = [0u8; 4096];
    let mut timed_out = false;
    loop {
        match std::io::Read::read(&mut stream, &mut temp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&temp[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => { timed_out = true; break; }
            Err(_) => break,
        }
    }
    // A timeout with zero bytes received is a FAILED round-trip, not an empty
    // result set. Returning Ok("") here made `list-windows`/`ls` report zero
    // windows on a merely-slow server (silent wrong answer). Surface it as a
    // retryable error so the caller can retry or report honestly.
    if timed_out && buf.is_empty() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "no response from server (timed out)"));
    }
    let result = String::from_utf8_lossy(&buf).to_string();
    // Strip the "OK\n" AUTH response prefix if present
    let result = if result.starts_with("OK\n") {
        result[3..].to_string()
    } else if result.starts_with("OK\r\n") {
        result[4..].to_string()
    } else {
        result
    };
    Ok(result)
}

/// Same as [`send_control_with_response`] but with caller-supplied connect and
/// read timeouts, so long-lived requests like `wait-event` can be given a
/// read deadline that comfortably exceeds the requested wait, while short
/// requests (e.g. `notify`) can still be tightly bounded (2s).
///
/// `total_ms` drives both timeouts: the connect timeout is `min(1000, total_ms)`
/// (a slow/saturated listener shouldn't be allowed to eat the whole budget),
/// and the read timeout is `total_ms` (bounds the full round trip, matching
/// the semantics `send_control_with_response` documents above).
pub fn send_control_with_response_timeout(line: String, total_ms: u64) -> io::Result<String> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let mut target = env::var("PSMUX_TARGET_SESSION").ok().unwrap_or_else(|| "default".to_string());
    // Never target a warm (standby) session — resolve to a real session instead
    if is_warm_session(&target) {
        let ns = target.strip_suffix("____warm__").map(|s| s.to_string());
        target = resolve_last_session_name_ns(ns.as_deref()).unwrap_or_else(|| "default".to_string());
    }
    let full_target = env::var("PSMUX_TARGET_FULL").ok();
    let path = format!("{}\\.psmux\\{}.port", home, target);
    let port = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u16>().ok()).ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("no server running on session '{}'", target)))?.clone();
    let session_key = read_session_key(&target).unwrap_or_default();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "bad server address"))?;
    let connect_ms = total_ms.min(1000);
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(connect_ms))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(total_ms)));
    let _ = write!(stream, "AUTH {}\n", session_key);
    if let Some(ref ft) = full_target {
        let _ = write!(stream, "TARGET {}\n", ft);
    }
    let _ = write!(stream, "{}", line);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = Vec::new();
    let mut temp = [0u8; 4096];
    let mut timed_out = false;
    loop {
        match std::io::Read::read(&mut stream, &mut temp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&temp[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => { timed_out = true; break; }
            Err(_) => break,
        }
    }
    if timed_out && buf.is_empty() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "no response from server (timed out)"));
    }
    let result = String::from_utf8_lossy(&buf).to_string();
    let result = if result.starts_with("OK\n") {
        result[3..].to_string()
    } else if result.starts_with("OK\r\n") {
        result[4..].to_string()
    } else {
        result
    };
    Ok(result)
}

/// Core read loop of [`stream_control_lines`], factored over any `BufRead`
/// so it can be unit-tested without a live TCP server. Skips `lines_to_skip`
/// preamble lines, then invokes `on_line` per line (CR/LF-trimmed).
///
/// Returns `true` when the stream stopped because `on_line` returned false
/// (a CLEAN stop — the caller saw its terminal frame), and `false` when the
/// stream ended on its own — EOF, read timeout, or read error — before the
/// callback asked to stop (TRANSPORT LOSS from the caller's point of view).
pub(crate) fn stream_lines_until_stopped(
    reader: &mut impl std::io::BufRead,
    mut lines_to_skip: usize,
    mut on_line: impl FnMut(&str) -> bool,
) -> bool {
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => return false, // EOF before a clean stop
            Ok(_) => {
                let text = buf.trim_end_matches(['\r', '\n']);
                if lines_to_skip > 0 {
                    lines_to_skip -= 1;
                    continue;
                }
                if !on_line(text) { return true; }
            }
            Err(_) => return false, // timeout or transport error
        }
    }
}

/// Line-streaming variant used by `events`: sends the same AUTH/TARGET/command
/// preamble as [`send_control_with_response`], but does NOT half-close the
/// write side and does NOT read to EOF — instead reads line by line (the
/// server keeps this connection open, pushing one JSON event/heartbeat per
/// line) and invokes `on_line` for each line after the preamble is skipped.
///
/// Returns `Ok(true)` when streaming stopped because `on_line` returned false
/// (clean stop — the callback saw its terminal frame, e.g. `"type":"closed"`).
/// Returns `Ok(false)` when the stream ended WITHOUT the callback stopping it:
/// unexpected EOF, read timeout, or read error mid-stream (transport loss).
/// `Err` is returned only for setup failures (no server, connect failure).
pub fn stream_control_lines(
    line: String,
    on_line: impl FnMut(&str) -> bool,
) -> io::Result<bool> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).unwrap_or_default();
    let mut target = env::var("PSMUX_TARGET_SESSION").ok().unwrap_or_else(|| "default".to_string());
    if is_warm_session(&target) {
        let ns = target.strip_suffix("____warm__").map(|s| s.to_string());
        target = resolve_last_session_name_ns(ns.as_deref()).unwrap_or_else(|| "default".to_string());
    }
    let full_target = env::var("PSMUX_TARGET_FULL").ok();
    let path = format!("{}\\.psmux\\{}.port", home, target);
    let port = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u16>().ok()).ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("no server running on session '{}'", target)))?.clone();
    let session_key = read_session_key(&target).unwrap_or_default();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "bad server address"))?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(1000))?;
    let _ = stream.set_nodelay(true);
    // Long read timeout: the server sends a heartbeat at least every
    // HEARTBEAT_SECS, so any gap longer than that plus slack means the
    // connection is dead.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(crate::events::HEARTBEAT_SECS + 10)));
    let _ = write!(stream, "AUTH {}\n", session_key);
    if let Some(ref ft) = full_target {
        let _ = write!(stream, "TARGET {}\n", ft);
    }
    let _ = write!(stream, "{}", line);
    let _ = stream.flush();
    let mut reader = io::BufReader::new(stream);
    // Skip 2 preamble lines before any real event: the connection-level AUTH
    // "OK" ack, then the events-subscribe ack (the initial cursor/ack json).
    Ok(stream_lines_until_stopped(&mut reader, 2, on_line))
}

/// Send a control message to a specific port with authentication
pub fn send_control_to_port(port: u16, msg: &str, session_key: &str) -> io::Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    if let Ok(mut stream) = std::net::TcpStream::connect(&addr) {
        let _ = stream.set_nodelay(true);
        let _ = write!(stream, "AUTH {}\n", session_key);
        let _ = stream.write_all(msg.as_bytes());
        let _ = stream.flush();
        // Drain the OK response to prevent RST
        let mut buf = [0u8; 64];
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
        let _ = std::io::Read::read(&mut stream, &mut buf);
    }
    Ok(())
}

pub fn resolve_last_session_name() -> Option<String> {
    resolve_last_session_name_ns(None)
}

/// Resolve the most recently modified session, optionally filtered by -L namespace.
/// When `ns` is Some("foo"), only sessions with port files named "foo__*" are considered
/// and the returned name includes the prefix (e.g. "foo__dev").
/// When `ns` is None, only non-namespaced sessions (no "__" in name) are considered.
pub fn resolve_last_session_name_ns(ns: Option<&str>) -> Option<String> {
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).ok()?;
    resolve_last_session_name_ns_in(std::path::Path::new(&format!("{}\\.psmux", home)), ns)
}

/// Registry-directory-parameterized variant of [`resolve_last_session_name_ns`]:
/// the most recent real (non-warm) session base in namespace `ns`. Taking the
/// dir explicitly lets routing be unit-tested without mutating `USERPROFILE`/`HOME`.
pub fn resolve_last_session_name_ns_in(dir: &std::path::Path, ns: Option<&str>) -> Option<String> {
    let last = std::fs::read_to_string(dir.join("last_session")).ok();
    if let Some(name) = last {
        let name = name.trim().to_string();
        // Only accept the cached last_session if it matches the namespace filter
        let ns_ok = match ns {
            Some(n) => name.starts_with(&format!("{}__", n)),
            None => !name.contains("__"),
        };
        if ns_ok && dir.join(format!("{}.port", name)).exists() {
            return Some(name);
        }
    }
    let mut picks: Vec<(String, std::time::SystemTime)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Some(fname) = e.file_name().to_str() {
                if let Some((base, ext)) = fname.rsplit_once('.') {
                    if ext == "port" { if let Ok(md) = e.metadata() { picks.push((base.to_string(), md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH))); } }
                }
            }
        }
    }
    // Exclude warm (standby) sessions
    picks.retain(|(n, _)| !is_warm_session(n));
    // Filter by namespace: -L sessions have "ns__name" format
    picks.retain(|(n, _)| match ns {
        Some(prefix) => n.starts_with(&format!("{}__", prefix)),
        None => !n.contains("__"),
    });
    picks.sort_by_key(|(_, t)| *t);
    picks.last().map(|(n, _)| n.clone())
}

/// Resolve the routing target session (the port-file base name) for a CLI
/// command that did not name an explicit `-t session`.
///
/// Socket-selection precedence follows tmux: `-L`/`-S` take precedence over
/// `$TMUX`, which tmux consults only when neither is given (tmux.c `main()`:
/// `if (path == NULL && label == NULL)`). psmux has no single socket, so this
/// maps to: adopt the current server named by `$TMUX` only when its session is
/// in the requested `-L` namespace; otherwise resolve within the namespace (the
/// most-recent session, else the namespaced `X__default`).
///
/// `$TMUX` (set inside every psmux pane, formatted `<socketpath>,<port>,<idx>`)
/// identifies the current server via its control `<port>`; `psmux_dir` is the
/// `.psmux` registry directory of `<base>.port` files; `l_socket_name` is the
/// `-L` namespace, if any. Returns the base to route to, or `None`.
pub fn resolve_routing_target(
    l_socket_name: Option<&str>,
    tmux_env: Option<&str>,
    psmux_dir: &std::path::Path,
) -> Option<String> {
    // Adopt the current server (named by `$TMUX`) only when it is in-namespace.
    if let Some(tmux_val) = tmux_env {
        if let Some(base) = session_base_owning_tmux_port(tmux_val, psmux_dir) {
            let in_namespace = match l_socket_name {
                Some(ns) => base.starts_with(&format!("{}__", ns)),
                None => true,
            };
            if in_namespace {
                return Some(base);
            }
        }
    }
    // Otherwise the most recent real session in the namespace,
    if let Some(name) = resolve_last_session_name_ns_in(psmux_dir, l_socket_name) {
        return Some(name);
    }
    // else a namespaced `X__default` so `-L X` never leaks to the un-namespaced
    // `default` server. With no `-L`, stay unresolved (the caller keeps "default").
    l_socket_name.map(|ns| format!("{}__default", ns))
}

/// Find the session port-file base whose control port matches the one encoded
/// in a `$TMUX` value (`<socketpath>,<port>,<idx>`). Warm (standby) sessions are
/// internal-only and never adopted.
fn session_base_owning_tmux_port(tmux_val: &str, psmux_dir: &std::path::Path) -> Option<String> {
    let port: u16 = tmux_val.split(',').nth(1)?.trim().parse().ok()?;
    for entry in std::fs::read_dir(psmux_dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "port").unwrap_or(false) {
            if let Ok(port_str) = std::fs::read_to_string(&path) {
                if port_str.trim().parse::<u16>().ok() == Some(port) {
                    let base = path.file_stem().and_then(|s| s.to_str())?;
                    return if is_warm_session(base) { None } else { Some(base.to_string()) };
                }
            }
        }
    }
    None
}

pub fn resolve_default_session_name() -> Option<String> {
    if let Ok(name) = env::var("PSMUX_DEFAULT_SESSION") {
        let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).ok()?;
        let p = format!("{}\\.psmux\\{}.port", home, name);
        if std::path::Path::new(&p).exists() { return Some(name); }
    }
    let home = env::var("USERPROFILE").or_else(|_| env::var("HOME")).ok()?;
    let candidates = [format!("{}\\.psmuxrc", home), format!("{}\\.psmux\\pmuxrc", home)];
    for cfg in candidates.iter() {
        if let Ok(text) = std::fs::read_to_string(cfg) {
            let line = text.lines().find(|l| !l.trim().is_empty())?;
            let name = if let Some(rest) = line.strip_prefix("default-session ") { rest.trim().to_string() } else { line.trim().to_string() };
            let p = format!("{}\\.psmux\\{}.port", home, name);
            if std::path::Path::new(&p).exists() { return Some(name); }
        }
    }
    None
}

pub fn reap_children_placeholder() -> io::Result<bool> { Ok(false) }

/// Return the names of all live sessions by scanning .psmux/*.port files.
pub fn list_session_names() -> Vec<String> {
    list_session_names_ns(None)
}

/// Return session names filtered by namespace (same logic as resolve_last_session_name_ns).
pub fn list_session_names_ns(ns: Option<&str>) -> Vec<String> {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let dir = format!("{}\\.psmux", home);
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if let Some(fname) = e.file_name().to_str().map(|s| s.to_string()) {
                if let Some((base, ext)) = fname.rsplit_once('.') {
                    if ext == "port" {
                        if is_warm_session(base) { continue; }
                        // Filter by namespace
                        match ns {
                            Some(prefix) => {
                                if !base.starts_with(&format!("{}__", prefix)) { continue; }
                            }
                            None => {
                                if base.contains("__") { continue; }
                            }
                        }
                        names.push(base.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names
}

/// A tree entry used by choose-tree: either a session header or a window under a session.
#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub session_name: String,
    pub session_port: u16,
    pub is_session_header: bool,
    pub window_index: Option<usize>,
    pub window_name: String,
    pub window_panes: usize,
    pub window_size: String,
    pub is_current_session: bool,
    pub is_active_window: bool,
}

/// List all running sessions and their windows for choose-tree display.
/// Queries each running server via its TCP port for window list info.
pub fn list_all_sessions_tree(current_session: &str, current_windows: &[(String, usize, String, bool, usize)]) -> Vec<TreeEntry> {
    let home = match env::var("USERPROFILE").or_else(|_| env::var("HOME")) {
        Ok(h) => h,
        Err(_) => return vec![],
    };
    let psmux_dir = format!("{}\\.psmux", home);
    let mut sessions: Vec<(String, u16, std::time::SystemTime)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&psmux_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "port").unwrap_or(false) {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    // Hide warm (standby) sessions from choose-tree
                    if is_warm_session(stem) { continue; }
                    if let Ok(port_str) = std::fs::read_to_string(&path) {
                        if let Ok(port) = port_str.trim().parse::<u16>() {
                            let mtime = entry.metadata()
                                .and_then(|m| m.modified())
                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                            sessions.push((stem.to_string(), port, mtime));
                        }
                    }
                }
            }
        }
    }

    sessions.sort_by_key(|(name, _, _)| name.clone());

    let mut tree = Vec::new();
    for (name, port, _) in &sessions {
        let is_current = name == current_session;
        // Session header
        tree.push(TreeEntry {
            session_name: name.clone(),
            session_port: *port,
            is_session_header: true,
            window_index: None,
            window_name: String::new(),
            window_panes: 0,
            window_size: String::new(),
            is_current_session: is_current,
            is_active_window: false,
        });

        if is_current {
            // Use local data for the current session (fast, no IPC)
            for (wname, panes, size, is_active, disp_idx) in current_windows.iter() {
                tree.push(TreeEntry {
                    session_name: name.clone(),
                    session_port: *port,
                    is_session_header: false,
                    window_index: Some(*disp_idx),
                    window_name: wname.clone(),
                    window_panes: *panes,
                    window_size: size.clone(),
                    is_current_session: true,
                    is_active_window: *is_active,
                });
            }
        } else {
            // Query remote session for its window list
            let key = read_session_key(name).unwrap_or_default();
            let addr = format!("127.0.0.1:{}", port);
            if let Ok(resp) = send_auth_cmd_response(&addr, &key, b"list-windows -F \"#{window_index}:#{window_name}:#{window_panes}:#{window_width}x#{window_height}:#{window_active}\"\n") {
                for line in resp.lines() {
                    let line = line.trim();
                    if line.is_empty() { continue; }
                    let parts: Vec<&str> = line.splitn(5, ':').collect();
                    if parts.len() >= 5 {
                        let wi = parts[0].parse::<usize>().unwrap_or(0);
                        let wn = parts[1].to_string();
                        let wp = parts[2].parse::<usize>().unwrap_or(1);
                        let ws = parts[3].to_string();
                        let wa = parts[4] == "1";
                        tree.push(TreeEntry {
                            session_name: name.clone(),
                            session_port: *port,
                            is_session_header: false,
                            window_index: Some(wi),
                            window_name: wn,
                            window_panes: wp,
                            window_size: ws,
                            is_current_session: false,
                            is_active_window: wa,
                        });
                    }
                }
            }
        }
    }
    tree
}

/// Force-kill any remaining psmux/pmux/tmux server processes that didn't
/// exit via the TCP kill-server command.  This is the nuclear fallback that
/// guarantees kill-server always succeeds.
///
/// On Windows, uses CreateToolhelp32Snapshot to enumerate processes and
/// TerminateProcess to kill them.  Skips the current process.
#[cfg(windows)]
pub fn kill_remaining_server_processes() {
    const TH32CS_SNAPPROCESS: u32 = 0x00000002;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const INVALID_HANDLE: isize = -1;

    #[repr(C)]
    struct PROCESSENTRY32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> isize;
        fn Process32FirstW(h_snapshot: isize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn Process32NextW(h_snapshot: isize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn TerminateProcess(h_process: isize, exit_code: u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    let my_pid = std::process::id();

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE || snap == 0 { return; }

        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dw_size = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let target_names: &[&str] = &["psmux.exe", "pmux.exe", "tmux.exe"];
        let mut pids_to_kill: Vec<u32> = Vec::new();

        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let pid = pe.th32_process_id;
                if pid != my_pid {
                    // Extract exe name from wide string
                    let len = pe.sz_exe_file.iter().position(|&c| c == 0).unwrap_or(260);
                    let name = String::from_utf16_lossy(&pe.sz_exe_file[..len]);
                    let name_lower = name.to_lowercase();
                    for target in target_names {
                        if name_lower == *target || name_lower.ends_with(&format!("\\{}", target)) {
                            pids_to_kill.push(pid);
                            break;
                        }
                    }
                }
                if Process32NextW(snap, &mut pe) == 0 { break; }
            }
        }
        CloseHandle(snap);

        for pid in &pids_to_kill {
            let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, *pid);
            if h != 0 && h != INVALID_HANDLE {
                let _ = TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn kill_remaining_server_processes() {
    // On non-Windows, use signal-based killing
    let _ = std::process::Command::new("pkill")
        .args(&["-f", "psmux|pmux"])
        .status();
}

#[cfg(test)]
#[path = "../tests-rs/test_session.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests-rs/test_issue250_root_cause.rs"]
mod tests_issue250_root_cause;

#[cfg(test)]
#[path = "../tests-rs/test_session_id_alloc_race.rs"]
mod tests_session_id_alloc_race;

#[cfg(test)]
#[path = "../tests-rs/test_issue448_orphan_reaper.rs"]
mod tests_issue448_orphan_reaper;

#[cfg(test)]
#[path = "../tests-rs/test_startup_stale_port_tax.rs"]
mod tests_startup_stale_port_tax;

#[cfg(test)]
#[path = "../tests-rs/test_l_socket_tmux_precedence.rs"]
mod tests_l_socket_tmux_precedence;
