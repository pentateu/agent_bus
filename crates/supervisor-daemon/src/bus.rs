//! The internal event bus (§4.18): a `tokio::sync::broadcast` channel with the
//! tagged [`BusEvent`] enum from core.
//!
//! One broadcast channel for the whole daemon. Services subscribe with their
//! own buffer; a slow consumer that falls behind gets its `Lagged` error and
//! must resync from the store (which is why the store is journal-first).

use std::sync::Arc;

use supervisor_core::event::BusEvent;
use tokio::sync::broadcast;

/// A live broadcast channel of internal events.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<BusEvent>,
}

/// How many events a slow subscriber may fall behind before it starts
/// dropping (4096, matching cmux's event retention).
const BUFFER: usize = 4096;

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    /// Create a new bus.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUFFER);
        Self { tx }
    }

    /// Publish an event. Fails silently when there are no subscribers.
    pub fn publish(&self, event: BusEvent) {
        let _ = self.tx.send(event);
    }

    /// A subscriber that returns the next event (or a `Lagged` note if it fell
    /// behind).
    #[must_use]
    pub fn subscribe(&self) -> Receiver {
        Receiver { rx: self.tx.subscribe() }
    }

    /// The number of active subscribers (for dashboards).
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// A bus subscriber.
pub struct Receiver {
    rx: broadcast::Receiver<BusEvent>,
}

impl Receiver {
    /// Wait for the next event.
    ///
    /// # Errors
    /// [`RecvError::Lagged`] if this subscriber fell behind; [`RecvError::Closed`]
    /// if the bus is shutting down.
    pub async fn recv(&mut self) -> Result<BusEvent, RecvError> {
        self.rx.recv().await.map_err(|e| match e {
            broadcast::error::RecvError::Lagged(n) => RecvError::Lagged(n),
            broadcast::error::RecvError::Closed => RecvError::Closed,
        })
    }

    /// Receive for a service loop. A `Lagged` note (this subscriber fell
    /// behind while its handler was busy) is logged and the wait retried —
    /// the service resyncs instead of dying. `None` means the bus is closed
    /// (the daemon is shutting down) and the service should exit.
    /// Previously every service treated the first `Lagged` as fatal and
    /// exited silently, killing delivery/ACK resolution until a manual
    /// restart (review C-3).
    #[must_use]
    pub async fn recv_or_shutdown(&mut self) -> Option<BusEvent> {
        loop {
            match self.recv().await {
                Ok(event) => return Some(event),
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "bus subscriber fell behind; resyncing");
                }
                Err(RecvError::Closed | RecvError::Empty) => return None,
            }
        }
    }

    /// Try to take the next event without waiting.
    ///
    /// # Errors
    /// [`RecvError::Empty`] when nothing is ready; [`RecvError::Lagged`] if
    /// this subscriber fell behind; [`RecvError::Closed`] on shutdown.
    pub fn try_recv(&mut self) -> Result<BusEvent, RecvError> {
        self.rx.try_recv().map_err(|e| match e {
            broadcast::error::TryRecvError::Lagged(n) => RecvError::Lagged(n),
            broadcast::error::TryRecvError::Closed => RecvError::Closed,
            broadcast::error::TryRecvError::Empty => RecvError::Empty,
        })
    }
}

/// Why a receive failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvError {
    /// The subscriber fell behind by `n` events; resync from the store.
    Lagged(u64),
    /// The bus was closed (the daemon is shutting down).
    Closed,
    /// `try_recv` found nothing ready.
    Empty,
}

/// The shared bus handle used by services.
pub type SharedBus = Arc<Bus>;

/// A convenience constructor for `Arc<Bus>`.
#[must_use]
pub fn shared() -> SharedBus {
    Arc::new(Bus::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use supervisor_core::event::BusEvent;
    use supervisor_core::signal::Signal;

    #[tokio::test]
    async fn events_reach_subscribers() {
        let bus = Bus::new();
        let mut sub = bus.subscribe();
        bus.publish(BusEvent::Signal(Signal::SessionIdle {
            ws: "iot".to_owned(),
            agent: "dev_01".to_owned(),
        }));
        let e = sub.recv().await.unwrap();
        assert!(matches!(e, BusEvent::Signal(Signal::SessionIdle { .. })));
    }

    #[tokio::test]
    async fn try_recv_reports_empty() {
        let bus = Bus::new();
        let mut sub = bus.subscribe();
        assert_eq!(sub.try_recv(), Err(RecvError::Empty));
    }

    #[tokio::test]
    async fn slow_subscriber_lags() {
        let bus = Bus::new();
        let mut sub = bus.subscribe();
        for i in 0..(BUFFER + 10) {
            bus.publish(BusEvent::Decision(supervisor_core::types::DecisionRecord {
                id: format!("d{i}"),
                signature: "sig".to_owned(),
                situation: serde_json::json!({}),
                decision: serde_json::json!({}),
                outcome: None,
                ts: "t".to_owned(),
            }));
        }
        match sub.recv().await {
            Err(RecvError::Lagged(n)) => assert!(n > 0),
            other => panic!("expected a lag error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_each_get_every_event() {
        let bus = Bus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.publish(BusEvent::Signal(Signal::Heartbeat { ws: "iot".to_owned() }));
        assert!(matches!(a.recv().await.unwrap(), BusEvent::Signal(_)));
        assert!(matches!(b.recv().await.unwrap(), BusEvent::Signal(_)));
        assert!(bus.subscriber_count() >= 2);
    }
}
