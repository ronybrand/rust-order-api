//! The publisher and the consumer against a real `RabbitMQ`: the topology, the confirms, the retry with its header, the
//! dead letter queue and the deduplication.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use lapin::options::{BasicGetOptions, BasicNackOptions, BasicPublishOptions, QueuePurgeOptions};
use lapin::types::AMQPValue;
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use orderapi::app::EventPublisher;
use orderapi::domain::{OrderStatus, OrderStatusChanged};
use orderapi::http::system_millis;
use orderapi::messaging::processor::{DLQ_NAME, QUEUE_NAME};
use orderapi::messaging::{Notifier, NotifyError, Processor, RabbitConsumer, RabbitPublisher};
use rust_decimal::Decimal;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::common::service;

/// The queues are global to the broker, so these tests run one at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

/// Records what it is given and fails as scripted.
struct Recording {
    seen: StdMutex<Vec<OrderStatusChanged>>,
    failure: Option<NotifyError>,
}

impl Recording {
    fn new(failure: Option<NotifyError>) -> Arc<Self> {
        Arc::new(Self { seen: StdMutex::new(Vec::new()), failure })
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl Notifier for Recording {
    async fn notify(&self, event: &OrderStatusChanged) -> Result<(), NotifyError> {
        self.seen.lock().unwrap().push(event.clone());
        self.failure.clone().map_or(Ok(()), Err)
    }
}

fn event() -> OrderStatusChanged {
    OrderStatusChanged {
        order_id: Uuid::new_v4(),
        customer_email: "ada@example.com".into(),
        customer_name: "Ada Lovelace".into(),
        old_status: OrderStatus::Open,
        new_status: OrderStatus::Confirmed,
        total_amount: Decimal::new(4028, 2),
        changed_at: OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    }
}

async fn channel(url: &str) -> (Connection, Channel) {
    let connection = Connection::connect(url, ConnectionProperties::default()).await.expect("the broker");
    let channel = connection.create_channel().await.expect("a channel");
    (connection, channel)
}

/// A clean broker for a test: the topology exists (the publisher declares it) and both queues are empty.
async fn prepare(url: &str) -> (Connection, Channel) {
    // Declaring the queues is the publisher's job; a first publish here would leave a message, so purge afterwards.
    let publisher = RabbitPublisher::new(url);
    let _ = publisher.publish_status_changed(&event()).await;
    publisher.close().await;
    let (connection, channel) = channel(url).await;
    for queue in [QUEUE_NAME, DLQ_NAME] {
        channel.queue_purge(queue.into(), QueuePurgeOptions::default()).await.expect("purge");
    }
    (connection, channel)
}

fn start_consumer(url: &str, notifier: Arc<Recording>) -> tokio::task::JoinHandle<()> {
    let url = url.to_owned();
    tokio::spawn(async move {
        let consumer = RabbitConsumer::new(url, move || {
            Processor::new(Arc::clone(&notifier) as Arc<dyn Notifier>, system_millis())
        })
        .with_retry_delay(Duration::from_millis(50));
        consumer.run(std::future::pending::<()>()).await;
    })
}

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn take(channel: &Channel, queue: &str) -> Option<lapin::message::BasicGetMessage> {
    for _ in 0..100 {
        if let Some(message) = channel.basic_get(queue.into(), BasicGetOptions { no_ack: true }).await.expect("get") {
            return Some(message);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

#[tokio::test]
async fn a_published_event_reaches_the_notifier_intact() {
    let Some(url) = service("TEST_AMQP_URL", "a_published_event_reaches_the_notifier_intact") else { return };
    let _guard = ONE_AT_A_TIME.lock().await;
    let (_connection, _channel) = prepare(&url).await;
    let notifier = Recording::new(None);
    let consumer = start_consumer(&url, Arc::clone(&notifier));

    let publisher = RabbitPublisher::new(&url);
    let sent = event();
    publisher.publish_status_changed(&sent).await.expect("published and confirmed");
    eventually("the notification", || notifier.count() == 1).await;

    let mut received = notifier.seen.lock().unwrap()[0].clone();
    received.total_amount.rescale(2);
    assert_eq!(received, sent);
    consumer.abort();
    publisher.close().await;
}

#[tokio::test]
async fn a_transient_failure_is_retried_with_the_count_in_the_header_and_then_dead_lettered() {
    let Some(url) = service("TEST_AMQP_URL", "a_transient_failure_is_retried") else { return };
    let _guard = ONE_AT_A_TIME.lock().await;
    let (_connection, channel) = prepare(&url).await;
    let notifier = Recording::new(Some(NotifyError::Transient("smtp down".into())));
    let consumer = start_consumer(&url, Arc::clone(&notifier));

    RabbitPublisher::new(&url).publish_status_changed(&event()).await.unwrap();

    // One try, then three retries, then the dead letter queue.
    let dead = take(&channel, DLQ_NAME).await.expect("the message in the dead letter queue");
    assert_eq!(notifier.count(), 4);
    let retries = dead.delivery.properties.headers().as_ref().and_then(|h| h.inner().get("x-retry-count").cloned());
    assert!(matches!(retries, Some(AMQPValue::LongInt(3))), "{retries:?}");
    assert!(dead.delivery.properties.message_id().is_some(), "the copy keeps its message id");
    consumer.abort();
}

#[tokio::test]
async fn a_permanent_failure_goes_straight_to_the_dead_letter_queue() {
    let Some(url) = service("TEST_AMQP_URL", "a_permanent_failure_goes_straight_to_the_dlq") else { return };
    let _guard = ONE_AT_A_TIME.lock().await;
    let (_connection, channel) = prepare(&url).await;
    let notifier = Recording::new(Some(NotifyError::Permanent("invalid recipient".into())));
    let consumer = start_consumer(&url, Arc::clone(&notifier));

    RabbitPublisher::new(&url).publish_status_changed(&event()).await.unwrap();

    assert!(take(&channel, DLQ_NAME).await.is_some());
    assert_eq!(notifier.count(), 1, "no retry for a permanent failure");
    consumer.abort();
}

#[tokio::test]
async fn a_malformed_message_is_dead_lettered_and_the_consumer_keeps_working() {
    let Some(url) = service("TEST_AMQP_URL", "a_malformed_message_is_dead_lettered") else { return };
    let _guard = ONE_AT_A_TIME.lock().await;
    let (_connection, channel) = prepare(&url).await;
    let notifier = Recording::new(None);
    let consumer = start_consumer(&url, Arc::clone(&notifier));

    channel
        .basic_publish(
            "".into(),
            QUEUE_NAME.into(),
            BasicPublishOptions::default(),
            b"not json at all",
            BasicProperties::default(),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    RabbitPublisher::new(&url).publish_status_changed(&event()).await.unwrap();

    let junk = take(&channel, DLQ_NAME).await.expect("the junk in the dead letter queue");
    assert_eq!(junk.delivery.data, b"not json at all");
    eventually("the valid message after the junk", || notifier.count() == 1).await;
    consumer.abort();
}

#[tokio::test]
async fn the_queues_are_durable_and_the_main_queue_dead_letters_to_the_dlq() {
    let Some(url) = service("TEST_AMQP_URL", "the_queues_are_durable") else { return };
    let _guard = ONE_AT_A_TIME.lock().await;
    let (_connection, channel) = prepare(&url).await;

    channel
        .basic_publish(
            "".into(),
            QUEUE_NAME.into(),
            BasicPublishOptions::default(),
            b"rejected",
            BasicProperties::default().with_delivery_mode(2),
        )
        .await
        .unwrap()
        .await
        .unwrap();
    let message = take_unacked(&channel, QUEUE_NAME).await;
    // Rejecting without requeue sends it through the dead-letter exchange configured on the main queue.
    message.delivery.nack(BasicNackOptions { requeue: false, ..BasicNackOptions::default() }).await.unwrap();
    let dead = take(&channel, DLQ_NAME).await.expect("dead-lettered by the broker");
    assert_eq!(dead.delivery.data, b"rejected");
}

async fn take_unacked(channel: &Channel, queue: &str) -> lapin::message::BasicGetMessage {
    for _ in 0..100 {
        if let Some(message) = channel.basic_get(queue.into(), BasicGetOptions { no_ack: false }).await.unwrap() {
            return message;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no message in {queue}");
}

#[tokio::test]
async fn a_publisher_that_cannot_reach_the_broker_reports_a_failure_without_quoting_the_url() {
    let publisher = RabbitPublisher::new("amqp://guest:topsecret@127.0.0.1:1/");
    let failure = publisher.publish_status_changed(&event()).await.unwrap_err();
    assert!(!failure.to_string().contains("topsecret"), "{failure}");
}
