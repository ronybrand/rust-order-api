use std::time::Duration;

use async_trait::async_trait;
use lapin::BasicProperties;
use tracing::error;

use super::processor::{Action, DLQ_NAME, Message, Processor, QUEUE_NAME, RETRY_DELAY_MILLIS};

/// A delivery from the broker, reduced to what the handler needs.
#[derive(Debug, Clone)]
pub struct Delivered {
    pub message_id: Option<String>,
    pub body: Vec<u8>,
    pub retries: u32,
    /// The original properties, kept so a moved copy has the same content type, timestamp and headers.
    pub properties: Option<BasicProperties>,
}

/// What the consumer does with the delivery after the handler decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ack,
    Requeue,
}

/// A copy could not be confirmed by the broker. The text is for the log.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MoveError(pub String);

/// Moves a delivery to another queue and returns only once the broker has confirmed it.
#[async_trait]
pub trait Mover: Send + Sync {
    async fn move_to(&self, queue: &str, delivery: &Delivered, retries: u32) -> Result<(), MoveError>;
}

/// The consumer's decision logic, free of the AMQP connection: apply the [`Processor`]'s verdict, move the message
/// when it must be retried or dead-lettered, and report whether the original may be acknowledged.
///
/// A message is never acknowledged before its copy is confirmed, so a failure while moving it puts it back instead of
/// losing it.
pub struct DeliveryHandler<M> {
    processor: Processor,
    mover: M,
    retry_delay: Duration,
}

impl<M> std::fmt::Debug for DeliveryHandler<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeliveryHandler")
    }
}

impl<M: Mover> DeliveryHandler<M> {
    pub fn new(processor: Processor, mover: M) -> Self {
        Self { processor, mover, retry_delay: Duration::from_millis(RETRY_DELAY_MILLIS) }
    }

    #[must_use]
    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    pub async fn handle(&self, delivery: &Delivered) -> Outcome {
        let message =
            Message { id: delivery.message_id.clone(), body: delivery.body.clone(), retries: delivery.retries };
        let action = self.processor.process(&message).await;
        let moved = match action {
            Action::Ack => Ok(()),
            Action::Retry => {
                tokio::time::sleep(self.retry_delay).await;
                self.mover.move_to(QUEUE_NAME, delivery, delivery.retries + 1).await
            }
            Action::DeadLetter => self.mover.move_to(DLQ_NAME, delivery, delivery.retries).await,
        };
        match moved {
            Ok(()) => Outcome::Ack,
            // Whatever goes wrong while moving the message (a closed channel, a timeout, a nack from the broker), the
            // answer is the same: put the original back instead of acknowledging it.
            Err(failure) => {
                error!(message_id = ?delivery.message_id, ?action, reason = %failure, "could not move message, requeueing");
                Outcome::Requeue
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rust_decimal::Decimal;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::domain::{OrderStatus, OrderStatusChanged};
    use crate::messaging::event_wire;
    use crate::messaging::processor::{Notifier, NotifyError};

    struct Fixed(Mutex<Vec<Result<(), NotifyError>>>);

    #[async_trait]
    impl Notifier for Fixed {
        async fn notify(&self, _: &OrderStatusChanged) -> Result<(), NotifyError> {
            let mut results = self.0.lock().unwrap();
            if results.is_empty() { Ok(()) } else { results.remove(0) }
        }
    }

    #[derive(Clone, Default)]
    struct Recorder {
        moves: Arc<Mutex<Vec<(String, u32)>>>,
        fail: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl Mover for Recorder {
        async fn move_to(&self, queue: &str, _: &Delivered, retries: u32) -> Result<(), MoveError> {
            if *self.fail.lock().unwrap() {
                return Err(MoveError("the broker did not confirm".into()));
            }
            self.moves.lock().unwrap().push((queue.to_owned(), retries));
            Ok(())
        }
    }

    fn delivery(retries: u32, body: Option<Vec<u8>>) -> Delivered {
        let body = body.unwrap_or_else(|| {
            event_wire::encode(&OrderStatusChanged {
                order_id: Uuid::new_v4(),
                customer_email: "ada@example.com".into(),
                customer_name: "Ada".into(),
                old_status: OrderStatus::Open,
                new_status: OrderStatus::Confirmed,
                total_amount: Decimal::new(1000, 2),
                changed_at: OffsetDateTime::UNIX_EPOCH,
            })
        });
        Delivered { message_id: Some("m1".into()), body, retries, properties: None }
    }

    fn handler(results: Vec<Result<(), NotifyError>>, mover: &Recorder) -> DeliveryHandler<Recorder> {
        let processor = Processor::new(Arc::new(Fixed(Mutex::new(results))), Arc::new(|| 0));
        DeliveryHandler::new(processor, mover.clone()).with_retry_delay(Duration::from_millis(5))
    }

    #[tokio::test]
    async fn a_delivered_message_is_acknowledged_and_moved_nowhere() {
        let mover = Recorder::default();
        assert_eq!(handler(vec![], &mover).handle(&delivery(0, None)).await, Outcome::Ack);
        assert!(mover.moves.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_transient_failure_is_republished_to_the_main_queue_with_the_count_up_by_one() {
        let mover = Recorder::default();
        let outcome = handler(vec![Err(NotifyError::Transient("x".into()))], &mover).handle(&delivery(1, None)).await;
        assert_eq!(outcome, Outcome::Ack);
        assert_eq!(*mover.moves.lock().unwrap(), [("order.status.changed".to_owned(), 2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_waits_for_the_delay_before_republishing() {
        let mover = Recorder::default();
        let processor =
            Processor::new(Arc::new(Fixed(Mutex::new(vec![Err(NotifyError::Transient("x".into()))]))), Arc::new(|| 0));
        let handler = DeliveryHandler::new(processor, mover.clone());
        let started = tokio::time::Instant::now();
        handler.handle(&delivery(0, None)).await;
        assert!(started.elapsed() >= Duration::from_millis(2_000));
    }

    #[tokio::test]
    async fn a_permanent_failure_and_a_malformed_message_go_to_the_dead_letter_queue_with_the_count_unchanged() {
        let mover = Recorder::default();
        handler(vec![Err(NotifyError::Permanent("bad recipient".into()))], &mover).handle(&delivery(2, None)).await;
        handler(vec![], &mover).handle(&delivery(0, Some(b"junk".to_vec()))).await;
        assert_eq!(
            *mover.moves.lock().unwrap(),
            [("order.status.changed.dlq".to_owned(), 2), ("order.status.changed.dlq".to_owned(), 0)]
        );
    }

    #[tokio::test]
    async fn when_the_copy_cannot_be_confirmed_the_original_goes_back_to_the_queue() {
        let mover = Recorder::default();
        *mover.fail.lock().unwrap() = true;
        let retry = handler(vec![Err(NotifyError::Transient("x".into()))], &mover).handle(&delivery(0, None)).await;
        let dead = handler(vec![], &mover).handle(&delivery(0, Some(b"junk".to_vec()))).await;
        assert_eq!((retry, dead), (Outcome::Requeue, Outcome::Requeue));
    }
}
