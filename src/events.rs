use serde::Serialize;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};

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

pub const RING_CAP: usize = 4096;
pub const SUB_CHANNEL_CAP: usize = 8192; // > RING_CAP so replay can never block
pub const MAX_EVENT_BYTES: usize = 16 * 1024;
pub const HEARTBEAT_SECS: u64 = 15;
/// Hard cap on concurrent subscribers per bus (spec §4 honesty: v1 ships a
/// fixed cap, not the deferred per-pane/global rate limiter). The 65th
/// concurrent `events`/`wait-event` caller is refused registration rather
/// than growing `subs` unbounded.
pub const MAX_SUBSCRIBERS: usize = 64;

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub session_uid: String,
    pub bus_id: String,
    pub seq: u64,
    pub name: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_instance: Option<u64>,
    pub occurred_at_ms: u128,
    pub payload: serde_json::Value,
}

pub enum SubscriberMsg {
    Event(Box<Event>),
    Heartbeat,
    Closed(&'static str),
}

struct Subscriber {
    tx: SyncSender<SubscriberMsg>,
    names: Vec<String>,
    categories: Vec<String>,
}

impl Subscriber {
    fn accepts(&self, e: &Event) -> bool {
        (self.names.is_empty() || self.names.iter().any(|n| n == &e.name))
            && (self.categories.is_empty() || self.categories.iter().any(|c| c == &e.category))
    }
}

pub struct SubscribeAck {
    pub ack_json: String,
    pub gap: bool,
    pub mismatch: bool,
    pub refused: bool,
}

pub struct EventBus {
    session_uid: String,
    bus_id: String,
    seq: u64,
    ring: VecDeque<Event>,
    subs: Vec<Subscriber>,
}

impl EventBus {
    pub fn new_dormant() -> EventBus {
        EventBus { session_uid: String::new(), bus_id: String::new(), seq: 0, ring: VecDeque::new(), subs: Vec::new() }
    }

    pub fn activate(&mut self, session_uid: String) {
        self.session_uid = session_uid;
        self.bus_id = gen_uid();
    }

    pub fn is_active(&self) -> bool { !self.bus_id.is_empty() }
    pub fn bus_id(&self) -> &str { &self.bus_id }
    pub fn session_uid(&self) -> &str { &self.session_uid }
    pub fn latest_seq(&self) -> u64 { self.seq }
    pub fn oldest_seq(&self) -> u64 { self.ring.front().map(|e| e.seq).unwrap_or(self.seq + 1) }
    pub fn subscriber_count(&self) -> usize { self.subs.len() }

    fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    pub fn publish(
        &mut self,
        name: &str,
        category: &str,
        pane: Option<usize>,
        pane_instance: Option<u64>,
        payload: serde_json::Value,
    ) -> Option<u64> {
        if !self.is_active() { return None; }
        self.seq += 1;
        let mut ev = Event {
            session_uid: self.session_uid.clone(),
            bus_id: self.bus_id.clone(),
            seq: self.seq,
            name: name.to_string(),
            category: category.to_string(),
            pane,
            pane_instance,
            occurred_at_ms: Self::now_ms(),
            payload,
        };
        if serde_json::to_string(&ev).map(|s| s.len()).unwrap_or(usize::MAX) > MAX_EVENT_BYTES {
            ev.payload = serde_json::json!({ "truncated": true });
        }
        if self.ring.len() == RING_CAP { self.ring.pop_front(); }
        self.ring.push_back(ev.clone());
        // fan out; drop slow consumers
        let mut i = 0;
        while i < self.subs.len() {
            if !self.subs[i].accepts(&ev) { i += 1; continue; }
            match self.subs[i].tx.try_send(SubscriberMsg::Event(Box::new(ev.clone()))) {
                Ok(()) => i += 1,
                Err(TrySendError::Full(_)) => {
                    let s = self.subs.swap_remove(i);
                    let _ = s.tx.try_send(SubscriberMsg::Closed("slow_consumer"));
                }
                Err(TrySendError::Disconnected(_)) => { self.subs.swap_remove(i); }
            }
        }
        Some(ev.seq)
    }

    pub fn subscribe(
        &mut self,
        names: Vec<String>,
        categories: Vec<String>,
        after: Option<Cursor>,
        tx: SyncSender<SubscriberMsg>,
    ) -> SubscribeAck {
        let mut gap = false;
        let mut gap_reason = String::new();
        let mut mismatch = false;
        let from_seq = match &after {
            None => self.seq, // live-only
            Some(c) => {
                if c.session_uid != self.session_uid || c.bus_id != self.bus_id {
                    mismatch = true;
                    self.seq
                } else if c.seq + 1 < self.oldest_seq() && c.seq < self.seq {
                    gap = true;
                    gap_reason = "cursor older than retained ring".to_string();
                    self.oldest_seq().saturating_sub(1)
                } else if c.seq > self.seq {
                    gap = true;
                    gap_reason = "cursor newer than latest".to_string();
                    self.seq
                } else {
                    c.seq
                }
            }
        };
        // Cap enforced before registration, not before the mismatch/gap
        // computation above: a mismatched cursor is already refused
        // registration on its own terms, and reporting mismatch takes
        // precedence over reporting "refused" for that case.
        let refused = !mismatch && self.subs.len() >= MAX_SUBSCRIBERS;
        let sub = Subscriber { tx, names, categories };
        let mut replay_count = 0u64;
        if !mismatch && !refused {
            for ev in self.ring.iter().filter(|e| e.seq > from_seq) {
                if sub.accepts(ev) {
                    // capacity SUB_CHANNEL_CAP > RING_CAP: cannot block on a fresh channel
                    let _ = sub.tx.try_send(SubscriberMsg::Event(Box::new(ev.clone())));
                    replay_count += 1;
                }
            }
        }
        let ack_json = serde_json::json!({
            "type": "ack",
            "session_uid": self.session_uid,
            "bus_id": self.bus_id,
            "oldest_seq": self.oldest_seq(),
            "latest_seq": self.seq,
            "replay_count": replay_count,
            "gap": if refused { false } else { gap },
            "gap_reason": gap_reason,
            "mismatch": mismatch,
            "refused": if refused { serde_json::Value::String("max_subscribers".to_string()) } else { serde_json::Value::Null },
        })
        .to_string();
        if !mismatch && !refused { self.subs.push(sub); }
        SubscribeAck { ack_json, gap, mismatch, refused }
    }

    /// Periodic heartbeat fan-out; call from the main loop every HEARTBEAT_SECS.
    pub fn heartbeat(&mut self) {
        let mut i = 0;
        while i < self.subs.len() {
            match self.subs[i].tx.try_send(SubscriberMsg::Heartbeat) {
                Ok(()) => i += 1,
                Err(TrySendError::Full(_)) => {
                    let s = self.subs.swap_remove(i);
                    let _ = s.tx.try_send(SubscriberMsg::Closed("slow_consumer"));
                }
                Err(TrySendError::Disconnected(_)) => { self.subs.swap_remove(i); }
            }
        }
    }

    pub fn close_all(&mut self, reason: &'static str) {
        for s in self.subs.drain(..) {
            let _ = s.tx.try_send(SubscriberMsg::Closed(reason));
        }
    }
}

#[cfg(test)]
#[path = "../tests-rs/test_agent_events_bus.rs"]
mod test_agent_events_bus;
