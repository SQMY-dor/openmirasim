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

    /// Subscribes to a session's live events. Done under the map lock, so the channel
    /// cannot be pruned between lookup and subscribe.
    pub(crate) fn subscribe(&self, id: &SessionId) -> broadcast::Receiver<SessionEvent> {
        let mut map = self
            .senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(id.clone())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .subscribe()
    }

    /// Broadcasts persisted events. A session's channel is dropped once nobody listens
    /// (review P1-10), so the map only holds sessions with subscribers; a later
    /// subscriber replays from storage anyway.
    pub(crate) fn publish(&self, events: &[SessionEvent]) {
        let Some(first) = events.first() else {
            return;
        };
        let mut map = self
            .senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(tx) = map.get(&first.session_id) else {
            return;
        };
        if tx.receiver_count() == 0 {
            map.remove(&first.session_id);
            return;
        }
        for ev in events {
            // No receivers is fine: events are already persisted.
            let _ = tx.send(ev.clone());
        }
    }

    #[cfg(test)]
    pub(crate) fn channels(&self) -> usize {
        self.senders.lock().map(|m| m.len()).unwrap_or(0)
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
        let rx = hub.subscribe(&session_id);
        let mut sub = Self {
            db,
            session_id,
            rx,
            backlog: VecDeque::new(),
            last_seq,
            head_seq: 0,
        };
        sub.refill().await?;
        if last_seq > sub.head_seq {
            // The client claims events this session never had (another server, a reset
            // database). Accepting it would silently skip every event up to `last_seq`.
            return Err(om_core::CoreError::InvalidInput(format!(
                "last_seq {last_seq} is ahead of the session head {}; resubscribe from 0",
                sub.head_seq
            )));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use om_core::event::EventPayload;

    fn ev(id: &SessionId, seq: u64) -> SessionEvent {
        SessionEvent {
            session_id: id.clone(),
            seq,
            ts: 0,
            payload: EventPayload::Error {
                message: "x".into(),
            },
        }
    }

    #[test]
    fn channels_without_listeners_are_dropped() {
        let hub = EventHub::new(8);
        let a = SessionId::new();
        hub.publish(&[ev(&a, 1)]);
        assert_eq!(hub.channels(), 0, "publishing alone creates nothing");
        let mut rx = hub.subscribe(&a);
        hub.publish(&[ev(&a, 2)]);
        assert_eq!(rx.try_recv().unwrap().seq, 2);
        drop(rx);
        hub.publish(&[ev(&a, 3)]);
        assert_eq!(
            hub.channels(),
            0,
            "the last listener leaving frees the channel"
        );
    }
}
