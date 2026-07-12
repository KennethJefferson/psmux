use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static UID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 24-hex-char unique id: 16 hex of nanos^pid hash + 8 hex process-local counter.
pub fn gen_uid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (pid << 32) ^ pid;
    let n = UID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:016x}{:08x}", mixed, n as u32)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    pub session_uid: String,
    pub bus_id: String,
    pub seq: u64,
}

impl Cursor {
    pub fn parse(s: &str) -> Option<Cursor> {
        let mut it = s.trim().split(':');
        let session_uid = it.next()?.to_string();
        let bus_id = it.next()?.to_string();
        let seq = it.next()?.parse::<u64>().ok()?;
        if it.next().is_some() || session_uid.is_empty() || bus_id.is_empty() {
            return None;
        }
        Some(Cursor { session_uid, bus_id, seq })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.session_uid, self.bus_id, self.seq)
    }
}
