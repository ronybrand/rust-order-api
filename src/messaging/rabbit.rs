//! `RabbitMQ`: the publisher the API uses and the consumer the worker runs.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, BasicNackOptions, BasicPublishOptions, BasicQosOptions, ConfirmSelectOptions,
    QueueDeclareOptions,
};
use lapin::types::{AMQPValue, FieldTable, ShortString};
use lapin::uri::AMQPUri;
use lapin::{BasicProperties, Channel, Confirmation, Connection, ConnectionProperties};
use tokio::sync::Mutex;
use tracing::{error, info};
use uuid::Uuid;

use super::delivery::{Delivered, DeliveryHandler, MoveError, Mover, Outcome};
use super::event_wire;
use super::processor::{DLQ_NAME, Processor, QUEUE_NAME, RETRY_COUNT_HEADER};
use crate::app::{EventPublisher, PublishError};
use crate::domain::OrderStatusChanged;

const TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const PERSISTENT: u8 = 2;

/// Opens a connection. `amqp://host/` is the default vhost `/` for every other client, and the URL of the compose
/// file; the URI parser reads it as the empty vhost, which no broker has: the broker closes the connection.
async fn connect(url: &str, name: &str) -> Result<Connection, String> {
    let mut uri: AMQPUri = url.parse().map_err(|_| "the broker URL is not valid".to_owned())?;
    if uri.vhost.is_empty() {
        uri.vhost = "/".into();
    }
    let properties = ConnectionProperties::default().with_connection_name(name.into());
    within(Connection::connect_uri(uri, properties), "the broker did not answer in time").await
}

/// Runs `step` with the usual timeout. The error texts are fixed: a driver message can quote the payload or the URL.
async fn within<T, E>(step: impl Future<Output = Result<T, E>>, timeout_message: &str) -> Result<T, String> {
    match tokio::time::timeout(TIMEOUT, step).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err("the broker refused or dropped the request".to_owned()),
        Err(_) => Err(timeout_message.to_owned()),
    }
}

/// Creates the queues idempotently: the main queue dead-letters to the DLQ through the default exchange, and both
/// survive a broker restart.
async fn declare_topology(channel: &Channel) -> Result<(), String> {
    within(
        channel.queue_declare(DLQ_NAME.into(), QueueDeclareOptions::durable(), FieldTable::default()),
        "declaring a queue timed out",
    )
    .await?;
    let mut arguments = FieldTable::default();
    arguments.insert("x-dead-letter-exchange".into(), AMQPValue::LongString("".into()));
    arguments.insert("x-dead-letter-routing-key".into(), AMQPValue::LongString(DLQ_NAME.into()));
    within(
        channel.queue_declare(QUEUE_NAME.into(), QueueDeclareOptions::durable(), arguments),
        "declaring a queue timed out",
    )
    .await?;
    Ok(())
}

/// Publishes one persistent message and returns only once the broker has taken responsibility for it.
async fn publish_confirmed(
    channel: &Channel,
    queue: &str,
    properties: BasicProperties,
    body: &[u8],
) -> Result<(), String> {
    let confirmation = within(
        async {
            channel
                .basic_publish("".into(), queue.into(), BasicPublishOptions::default(), body, properties)
                .await?
                .await
        },
        "the broker did not confirm in time",
    )
    .await?;
    match confirmation {
        Confirmation::Ack(_) => Ok(()),
        _ => Err("the broker did not accept the message".to_owned()),
    }
}

async fn open_channel(connection: &Connection) -> Result<Channel, String> {
    let channel = within(connection.create_channel(), "opening a channel timed out").await?;
    declare_topology(&channel).await?;
    // Publisher confirms: a publish is only successful once the broker has it.
    within(channel.confirm_select(ConfirmSelectOptions::default()), "enabling confirms timed out").await?;
    Ok(channel)
}

/// The [`EventPublisher`] backed by `RabbitMQ`. It dials lazily and redials after the connection drops, so an
/// unavailable broker at startup does not stop the API (publishing is best effort anyway, ADR 0003).
pub struct RabbitPublisher {
    url: String,
    link: Mutex<Option<(Connection, Channel)>>,
}

opaque_debug!(RabbitPublisher);

impl RabbitPublisher {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), link: Mutex::new(None) }
    }

    pub async fn close(&self) {
        if let Some((connection, _)) = self.link.lock().await.take() {
            let _ = connection.close(200, "shutdown".into()).await;
        }
    }
}

#[async_trait]
impl EventPublisher for RabbitPublisher {
    async fn publish_status_changed(&self, event: &OrderStatusChanged) -> Result<(), PublishError> {
        let properties = BasicProperties::default()
            .with_content_type("application/json".into())
            .with_delivery_mode(PERSISTENT)
            .with_message_id(Uuid::new_v4().to_string().into())
            .with_timestamp(u64::try_from(event.changed_at.unix_timestamp()).unwrap_or_default());
        let body = event_wire::encode(event);

        let mut link = self.link.lock().await;
        if !link
            .as_ref()
            .is_some_and(|(connection, channel)| connection.status().connected() && channel.status().connected())
        {
            let connection = connect(&self.url, "order-api-publisher").await.map_err(PublishError)?;
            let channel = match open_channel(&connection).await {
                Ok(channel) => channel,
                Err(reason) => {
                    let _ = connection.close(200, "setup failed".into()).await;
                    return Err(PublishError(reason));
                }
            };
            *link = Some((connection, channel));
        }
        let Some((_, channel)) = link.as_ref() else { return Err(PublishError("not connected".into())) };
        publish_confirmed(channel, QUEUE_NAME, properties, &body).await.map_err(PublishError)
    }
}

/// The number the `x-retry-count` header holds, whatever integer type the publisher used.
fn retries_of(properties: &BasicProperties) -> u32 {
    let value = properties.headers().as_ref().and_then(|headers| headers.inner().get(RETRY_COUNT_HEADER));
    match value {
        Some(AMQPValue::LongInt(n)) => u32::try_from(*n).unwrap_or(0),
        Some(AMQPValue::LongLongInt(n)) => u32::try_from(*n).unwrap_or(0),
        Some(AMQPValue::ShortInt(n)) => u32::try_from(*n).unwrap_or(0),
        Some(AMQPValue::ShortShortInt(n)) => u32::try_from(*n).unwrap_or(0),
        Some(AMQPValue::LongUInt(n)) => *n,
        Some(AMQPValue::ShortUInt(n)) => u32::from(*n),
        Some(AMQPValue::ShortShortUInt(n)) => u32::from(*n),
        _ => 0,
    }
}

/// Copies a delivery (same body and `message_id`, so dedupe still works) to another queue, confirmed by the broker.
struct RabbitMover {
    channel: Channel,
}

#[async_trait]
impl Mover for RabbitMover {
    async fn move_to(&self, queue: &str, delivery: &Delivered, retries: u32) -> Result<(), MoveError> {
        let original = delivery.properties.as_ref();
        let mut headers = original.and_then(|p| p.headers().clone()).unwrap_or_default();
        headers.insert(
            ShortString::from(RETRY_COUNT_HEADER),
            AMQPValue::LongInt(i32::try_from(retries).unwrap_or(i32::MAX)),
        );
        let mut properties = BasicProperties::default()
            .with_content_type(
                original.and_then(|p| p.content_type().clone()).unwrap_or_else(|| "application/json".into()),
            )
            .with_delivery_mode(PERSISTENT)
            .with_headers(headers);
        if let Some(id) = &delivery.message_id {
            properties = properties.with_message_id(id.as_str().into());
        }
        if let Some(timestamp) = original.and_then(|p| *p.timestamp()) {
            properties = properties.with_timestamp(timestamp);
        }
        publish_confirmed(&self.channel, queue, properties, &delivery.body).await.map_err(MoveError)
    }
}

/// Reads the queue and applies the [`DeliveryHandler`]'s decision to each delivery, reconnecting after a failure.
pub struct RabbitConsumer {
    url: String,
    processor: std::sync::Arc<dyn Fn() -> Processor + Send + Sync>,
    retry_delay: Duration,
}

opaque_debug!(RabbitConsumer);

impl RabbitConsumer {
    /// `processor` builds the processor for each connection, so its dedupe memory starts fresh with the connection.
    pub fn new(url: impl Into<String>, processor: impl Fn() -> Processor + Send + Sync + 'static) -> Self {
        Self {
            url: url.into(),
            processor: std::sync::Arc::new(processor),
            retry_delay: Duration::from_millis(super::processor::RETRY_DELAY_MILLIS),
        }
    }

    #[must_use]
    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    /// Consumes until `shutdown` completes, reconnecting after a broker failure. It never gives up on its own: a
    /// consumer that dies on the first unexpected error is a worker that silently stops sending e-mail.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                result = self.consume() => {
                    let reason = result.err().unwrap_or_else(|| "the consumer stopped".to_owned());
                    error!(reason, delay_ms = RECONNECT_DELAY.as_millis() as u64, "consumer stopped, reconnecting");
                    tokio::select! {
                        () = &mut shutdown => return,
                        () = tokio::time::sleep(RECONNECT_DELAY) => {}
                    }
                }
            }
        }
    }

    async fn consume(&self) -> Result<(), String> {
        let connection = connect(&self.url, "order-api-worker").await?;
        let channel = open_channel(&connection).await?;
        // One unacknowledged message at a time: the retry delay then really throttles the queue.
        within(channel.basic_qos(1, BasicQosOptions::default()), "setting the prefetch timed out").await?;
        let handler = DeliveryHandler::new((self.processor)(), RabbitMover { channel: channel.clone() })
            .with_retry_delay(self.retry_delay);
        let mut consumer = within(
            channel.basic_consume(
                QUEUE_NAME.into(),
                "order-api-worker".into(),
                BasicConsumeOptions::default(),
                FieldTable::default(),
            ),
            "starting to consume timed out",
        )
        .await?;
        info!(queue = QUEUE_NAME, "consuming");
        while let Some(next) = consumer.next().await {
            let delivery = next.map_err(|_| "the broker closed the consumer".to_owned())?;
            let delivered = Delivered {
                message_id: delivery.properties.message_id().as_ref().map(ToString::to_string),
                body: delivery.data.clone(),
                retries: retries_of(&delivery.properties),
                properties: Some(delivery.properties.clone()),
            };
            let acknowledged = match handler.handle(&delivered).await {
                Outcome::Ack => delivery.ack(BasicAckOptions::default()).await,
                Outcome::Requeue => {
                    delivery.nack(BasicNackOptions { requeue: true, ..BasicNackOptions::default() }).await
                }
            };
            acknowledged.map_err(|_| "the broker did not take the acknowledgement".to_owned())?;
        }
        Err("the broker closed the connection".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_header(value: AMQPValue) -> BasicProperties {
        let mut headers = FieldTable::default();
        headers.insert(RETRY_COUNT_HEADER.into(), value);
        BasicProperties::default().with_headers(headers)
    }

    #[test]
    fn the_retry_count_is_read_whatever_integer_type_the_header_has() {
        for value in [
            AMQPValue::LongInt(3),
            AMQPValue::LongLongInt(3),
            AMQPValue::ShortInt(3),
            AMQPValue::ShortShortInt(3),
            AMQPValue::LongUInt(3),
            AMQPValue::ShortUInt(3),
            AMQPValue::ShortShortUInt(3),
        ] {
            assert_eq!(retries_of(&with_header(value)), 3);
        }
    }

    #[test]
    fn a_missing_or_odd_retry_count_is_zero() {
        assert_eq!(retries_of(&BasicProperties::default()), 0);
        assert_eq!(retries_of(&with_header(AMQPValue::LongString("3".into()))), 0);
        assert_eq!(retries_of(&with_header(AMQPValue::LongInt(-1))), 0);
    }

    #[tokio::test]
    async fn a_broker_url_that_is_not_a_uri_is_refused_without_quoting_it() {
        let error = connect("definitely not a url", "test").await.unwrap_err();
        assert_eq!(error, "the broker URL is not valid");
    }

    #[tokio::test]
    async fn an_unreachable_broker_is_reported_with_a_fixed_text() {
        let error = connect("amqp://guest:secret@127.0.0.1:1/", "test").await.unwrap_err();
        assert!(!error.contains("secret") && !error.contains("127.0.0.1"), "{error}");
    }
}
