//! Per-session broadcast hub and gapless subscriptions.
//!
//! Ordering guarantee: the session actor persists every event before broadcasting it.
//! A [`Subscription`] subscribes to the broadcast *before* reading the backlog from
//! SQLite, so every event is either in the backlog or arrives live (possibly both;
//! duplicates are dropped by `seq`). If the receiver lags, missing events are re-read
//! from SQLite, so subscribers never observe a gap.

use crate::db::Db;
use om_core::CoreResult;
use om_core::event::SessionEvent;
use om_core::ids::SessionId;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::broadcast;

/// Page size used when replaying from SQLite.
const REPLAY_PAGE: u32 = 500;

pub(crate) struct EventHub {
    capacity: usize,
    senders: Mutex<HashMap<SessionId, broadcast::Sender<SessionEvent>>>,
}

impl EventHub {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            senders: Mutex::default(),
        }
    }

    pub(crate) fn sender(&self, id: &SessionId) -> broadcast::Sender<SessionEvent> {
        let mut map = self.senders.lock().expect("hub poisoned");
        map.entry(id.clone())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .clone()
    }

    pub(crate) fn publish(&self, events: &[SessionEvent]) {
        if let Some(first) = events.first() {
            let tx = self.sender(&first.session_id);
            for ev in events {
                // No receivers is fine: events are already persisted.
                let _ = tx.send(ev.clone());
            }
        }
    }
}

/// A gapless, ordered stream of one session's events starting after `last_seq`.
pub struct Subscription {
    db: Db,
    session_id: SessionId,
    rx: broadcast::Receiver<SessionEvent>,
    backlog: VecDeque<SessionEvent>,
    last_seq: u64,
    head_seq: u64,
}

impl Subscription {
    pub(crate) async fn open(
        db: Db,
        hub: &EventHub,
        session_id: SessionId,
        last_seq: u64,
    ) -> CoreResult<Self> {
        // Subscribe first, then read: see module docs.
        let rx = hub.sender(&session_id).subscribe();
        let mut sub = Self {
            db,
            session_id,
            rx,
            backlog: VecDeque::new(),
            last_seq,
            head_seq: 0,
        };
        sub.refill().await?;
        sub.head_seq = sub.head_seq.max(last_seq);
        Ok(sub)
    }

    /// Highest seq persisted when the subscription opened (for `Subscribed { head_seq }`).
    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    /// Seq of the last event this subscription yielded (or the starting `last_seq`).
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Re-reads everything after `last_seq` from SQLite into the backlog.
    async fn refill(&mut self) -> CoreResult<()> {
        let mut after = self.backlog.back().map(|e| e.seq).unwrap_or(self.last_seq);
        loop {
            let page = self
                .db
                .list_events(&self.session_id, after, REPLAY_PAGE)
                .await?;
            self.head_seq = self.head_seq.max(page.head_seq);
            if let Some(last) = page.events.last() {
                after = last.seq;
            }
            self.backlog.extend(page.events);
            if !page.has_more {
                return Ok(());
            }
        }
    }

    /// Next event in seq order. `Ok(None)` once the hub is gone (server shutdown).
    pub async fn next(&mut self) -> CoreResult<Option<SessionEvent>> {
        loop {
            if let Some(ev) = self.backlog.pop_front() {
                if ev.seq <= self.last_seq {
                    continue;
                }
                self.last_seq = ev.seq;
                return Ok(Some(ev));
            }
            match self.rx.recv().await {
                Ok(ev) if ev.seq <= self.last_seq => continue,
                Ok(ev) if ev.seq == self.last_seq + 1 => {
                    self.last_seq = ev.seq;
                    return Ok(Some(ev));
                }
                Ok(ev) => {
                    // Defensive: a gap should be impossible, but heal it from storage.
                    self.refill().await?;
                    self.backlog.push_back(ev);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(session = %self.session_id, skipped, "subscriber lagged; replaying from storage");
                    self.refill().await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    }
}
