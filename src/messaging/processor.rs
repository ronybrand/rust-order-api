use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::event_wire;
use crate::domain::OrderStatusChanged;
use crate::http::NowMillis;

// Queue topology and retry policy (DOMAIN.md section 5, ADR 0003).
pub const QUEUE_NAME: &str = "order.status.changed";
pub const DLQ_NAME: &str = "order.status.changed.dlq";
pub const MAX_RETRIES: u32 = 3;
pub const RETRY_DELAY_MILLIS: u64 = 2_000;
pub const RETRY_COUNT_HEADER: &str = "x-retry-count";
pub const SEEN_TTL_MILLIS: u64 = 3_600_000;
pub const SEEN_CAPACITY: usize = 10_000;

/// Why a notification could not be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyError {
    /// Retrying cannot fix it (an invalid recipient, say): the message goes straight to the DLQ.
    Permanent(String),
    /// A boundary to the outside world failed (a timeout, a refused connection): worth another try.
    Transient(String),
}

/// A delivery as the processor sees it, independent of the AMQP library.
#[derive(Debug, Clone)]
pub struct Message {
    /// The AMQP `message_id`, used to drop redeliveries.
    pub id: Option<String>,
    pub body: Vec<u8>,
    /// The value of the `x-retry-count` header.
    pub retries: u32,
}

/// What the consumer must do with the delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Done, or a duplicate: acknowledge.
    Ack,
    /// Transient failure: republish with the retry count incremented, after the retry delay.
    Retry,
    /// Unrecoverable: publish to the dead letter queue.
    DeadLetter,
}

/// Delivers the notification of one event.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, event: &OrderStatusChanged) -> Result<(), NotifyError>;
}

/// Decides what happens to each message. It owns no connection, so the retry, dedupe and dead letter rules are tested
/// without a broker.
///
/// Delivery is at-least-once, so a message already completed (same `message_id`) is acknowledged without notifying
/// again; only a successful notification marks the id as done.
pub struct Processor {
    notifier: Arc<dyn Notifier>,
    seen: Mutex<SeenSet>,
}

opaque_debug!(Processor);

impl Processor {
    pub fn new(notifier: Arc<dyn Notifier>, now: NowMillis) -> Self {
        Self { notifier, seen: Mutex::new(SeenSet::new(SEEN_TTL_MILLIS, SEEN_CAPACITY, now)) }
    }

    pub async fn process(&self, message: &Message) -> Action {
        let event = event_wire::decode(&message.body).filter(|event| event.order_id != Uuid::nil());
        let Some(event) = event else {
            error!(message_id = ?message.id, "discarding malformed message");
            return Action::DeadLetter;
        };
        if message.id.as_deref().is_some_and(|id| self.seen().has(id)) {
            info!(message_id = ?message.id, "duplicate message ignored");
            return Action::Ack;
        }
        self.deliver(message, &event).await
    }

    async fn deliver(&self, message: &Message, event: &OrderStatusChanged) -> Action {
        match self.notifier.notify(event).await {
            Ok(()) => {
                if let Some(id) = &message.id {
                    self.seen().add(id);
                }
                info!(event = ?event, "notification sent");
                Action::Ack
            }
            Err(NotifyError::Permanent(reason)) => {
                error!(event = ?event, reason, "notification failed permanently");
                Action::DeadLetter
            }
            // Any failure that is not permanent is transient by definition: the notifier is a boundary to the outside
            // world, and what it reports says nothing the processor could act on.
            Err(NotifyError::Transient(reason)) => {
                if message.retries >= MAX_RETRIES {
                    error!(event = ?event, reason, "notification failed, retries exhausted");
                    return Action::DeadLetter;
                }
                warn!(event = ?event, retries = message.retries, reason, "notification failed, will retry");
                Action::Retry
            }
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, SeenSet> {
        self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Remembers recently completed message ids: bounded in size and in time, so it cannot leak memory.
pub struct SeenSet {
    ttl_millis: u64,
    capacity: usize,
    now: NowMillis,
    entries: HashMap<String, u64>,
}

opaque_debug!(SeenSet);

impl SeenSet {
    pub fn new(ttl_millis: u64, capacity: usize, now: NowMillis) -> Self {
        Self { ttl_millis, capacity, now, entries: HashMap::new() }
    }

    pub fn has(&mut self, id: &str) -> bool {
        let Some(&at) = self.entries.get(id) else { return false };
        if (self.now)().saturating_sub(at) > self.ttl_millis {
            self.entries.remove(id);
            return false;
        }
        true
    }

    pub fn add(&mut self, id: &str) {
        let now = (self.now)();
        if self.entries.len() >= self.capacity {
            self.evict(now);
        }
        self.entries.insert(id.to_owned(), now);
    }

    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// Drops the expired entries and, if that was not enough, the oldest one.
    fn evict(&mut self, now: u64) {
        self.entries.retain(|_, at| now.saturating_sub(*at) <= self.ttl_millis);
        if self.entries.len() >= self.capacity
            && let Some(oldest) = self.entries.iter().min_by_key(|(_, at)| **at).map(|(id, _)| id.clone())
        {
            self.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use rust_decimal::Decimal;
    use time::OffsetDateTime;

    use super::*;
    use crate::domain::OrderStatus;

    struct Scripted {
        results: Mutex<Vec<Result<(), NotifyError>>>,
        sent: Mutex<Vec<OrderStatusChanged>>,
    }

    impl Scripted {
        fn new(results: Vec<Result<(), NotifyError>>) -> Arc<Self> {
            Arc::new(Self { results: Mutex::new(results), sent: Mutex::new(Vec::new()) })
        }

        fn calls(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Notifier for Scripted {
        async fn notify(&self, event: &OrderStatusChanged) -> Result<(), NotifyError> {
            self.sent.lock().unwrap().push(event.clone());
            let mut results = self.results.lock().unwrap();
            if results.is_empty() { Ok(()) } else { results.remove(0) }
        }
    }

    fn clock() -> (Arc<AtomicU64>, NowMillis) {
        let millis = Arc::new(AtomicU64::new(1_000));
        let reader = Arc::clone(&millis);
        (millis, Arc::new(move || reader.load(Ordering::SeqCst)))
    }

    fn body() -> Vec<u8> {
        event_wire::encode(&OrderStatusChanged {
            order_id: Uuid::new_v4(),
            customer_email: "ada@example.com".into(),
            customer_name: "Ada".into(),
            old_status: OrderStatus::Open,
            new_status: OrderStatus::Confirmed,
            total_amount: Decimal::new(1030, 2),
            changed_at: OffsetDateTime::UNIX_EPOCH,
        })
    }

    fn message(id: Option<&str>, retries: u32) -> Message {
        Message { id: id.map(str::to_owned), body: body(), retries }
    }

    fn processor(notifier: &Arc<Scripted>) -> (Processor, Arc<AtomicU64>) {
        let (millis, now) = clock();
        (Processor::new(Arc::clone(notifier) as Arc<dyn Notifier>, now), millis)
    }

    #[tokio::test]
    async fn a_delivered_notification_is_acknowledged() {
        let notifier = Scripted::new(vec![]);
        let (processor, _) = processor(&notifier);
        assert_eq!(processor.process(&message(Some("m1"), 0)).await, Action::Ack);
        assert_eq!(notifier.calls(), 1);
    }

    #[tokio::test]
    async fn a_redelivery_of_a_completed_message_is_acknowledged_without_notifying_again() {
        let notifier = Scripted::new(vec![]);
        let (processor, _) = processor(&notifier);
        processor.process(&message(Some("m1"), 0)).await;
        assert_eq!(processor.process(&message(Some("m1"), 0)).await, Action::Ack);
        assert_eq!(notifier.calls(), 1);
    }

    #[tokio::test]
    async fn the_dedupe_memory_expires() {
        let notifier = Scripted::new(vec![]);
        let (processor, millis) = processor(&notifier);
        processor.process(&message(Some("m1"), 0)).await;
        millis.fetch_add(SEEN_TTL_MILLIS + 1, Ordering::SeqCst);
        processor.process(&message(Some("m1"), 0)).await;
        assert_eq!(notifier.calls(), 2);
    }

    #[tokio::test]
    async fn a_message_without_an_id_is_never_deduplicated() {
        let notifier = Scripted::new(vec![]);
        let (processor, _) = processor(&notifier);
        processor.process(&message(None, 0)).await;
        processor.process(&message(None, 0)).await;
        assert_eq!(notifier.calls(), 2);
    }

    #[tokio::test]
    async fn only_a_success_marks_the_id_as_done() {
        let notifier = Scripted::new(vec![Err(NotifyError::Transient("smtp down".into()))]);
        let (processor, _) = processor(&notifier);
        assert_eq!(processor.process(&message(Some("m1"), 0)).await, Action::Retry);
        assert_eq!(processor.process(&message(Some("m1"), 1)).await, Action::Ack, "the retry is not a duplicate");
        assert_eq!(notifier.calls(), 2);
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_the_limit_and_then_dead_lettered() {
        let notifier = Scripted::new(vec![Err(NotifyError::Transient("x".into())); 4]);
        let (processor, _) = processor(&notifier);
        for retries in 0..MAX_RETRIES {
            assert_eq!(processor.process(&message(Some("m"), retries)).await, Action::Retry, "{retries}");
        }
        assert_eq!(processor.process(&message(Some("m"), MAX_RETRIES)).await, Action::DeadLetter);
    }

    #[tokio::test]
    async fn a_permanent_failure_goes_straight_to_the_dead_letter_queue() {
        let notifier = Scripted::new(vec![Err(NotifyError::Permanent("invalid recipient".into()))]);
        let (processor, _) = processor(&notifier);
        assert_eq!(processor.process(&message(Some("m"), 0)).await, Action::DeadLetter);
    }

    #[tokio::test]
    async fn a_malformed_message_is_dead_lettered_without_notifying() {
        let notifier = Scripted::new(vec![]);
        let (processor, _) = processor(&notifier);
        for body in [b"not json".to_vec(), Vec::new(), b"{}".to_vec()] {
            let message = Message { id: Some("m".into()), body, retries: 0 };
            assert_eq!(processor.process(&message).await, Action::DeadLetter);
        }
        let nil = event_wire::encode(&OrderStatusChanged {
            order_id: Uuid::nil(),
            customer_email: "a@b.co".into(),
            customer_name: "A".into(),
            old_status: OrderStatus::Open,
            new_status: OrderStatus::Canceled,
            total_amount: Decimal::ZERO,
            changed_at: OffsetDateTime::UNIX_EPOCH,
        });
        assert_eq!(processor.process(&Message { id: None, body: nil, retries: 0 }).await, Action::DeadLetter);
        assert_eq!(notifier.calls(), 0);
    }

    #[test]
    fn the_seen_set_is_bounded_in_size() {
        let (millis, now) = clock();
        let mut seen = SeenSet::new(1_000, 3, now);
        for (index, id) in ["a", "b", "c", "d"].iter().enumerate() {
            seen.add(id);
            millis.fetch_add(10 + index as u64, Ordering::SeqCst);
        }
        assert_eq!(seen.size(), 3);
        assert!(!seen.has("a"), "the oldest is the one dropped");
        assert!(seen.has("d"));
    }

    #[test]
    fn the_seen_set_drops_expired_entries_before_dropping_live_ones() {
        let (millis, now) = clock();
        let mut seen = SeenSet::new(100, 2, now);
        seen.add("old");
        millis.fetch_add(200, Ordering::SeqCst);
        seen.add("live");
        seen.add("new");
        assert!(seen.has("live") && seen.has("new") && !seen.has("old"));
    }
}
