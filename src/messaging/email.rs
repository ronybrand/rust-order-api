use async_trait::async_trait;
use rust_decimal::{Decimal, RoundingStrategy};
use time::OffsetDateTime;
use time::macros::format_description;

use super::processor::{Notifier, NotifyError};
use crate::domain::{OrderStatusChanged, is_valid_email};

const MONEY_SCALE: u32 = 2;

/// Why a mail was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailError {
    /// Retrying cannot fix it (an invalid header, a malformed address).
    Permanent(String),
    /// The server did not take it: worth another try.
    Transient(String),
}

/// Sends one HTML e-mail. The SMTP implementation is [`super::SmtpMailer`].
#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, to: &str, subject: &str, html_body: &str) -> Result<(), MailError>;
}

#[async_trait]
impl<M: Mailer + ?Sized> Mailer for std::sync::Arc<M> {
    async fn send(&self, to: &str, subject: &str, html_body: &str) -> Result<(), MailError> {
        (**self).send(to, subject, html_body).await
    }
}

/// Renders the status e-mail and hands it to a [`Mailer`].
pub struct EmailNotifier<M> {
    mailer: M,
}

impl<M> std::fmt::Debug for EmailNotifier<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EmailNotifier")
    }
}

impl<M: Mailer> EmailNotifier<M> {
    pub fn new(mailer: M) -> Self {
        Self { mailer }
    }
}

#[async_trait]
impl<M: Mailer> Notifier for EmailNotifier<M> {
    /// Validates the recipient (a bad one never becomes valid by retrying: permanent) and sends the mail. Any other
    /// failure of the mailer is transient and reaches the processor to be retried.
    async fn notify(&self, event: &OrderStatusChanged) -> Result<(), NotifyError> {
        if !is_valid_email(&event.customer_email) {
            return Err(NotifyError::Permanent("invalid recipient".into()));
        }
        let subject = format!("Order {}", event.new_status);
        self.mailer.send(&event.customer_email, &subject, &render(event)).await.map_err(|failure| match failure {
            MailError::Permanent(reason) => NotifyError::Permanent(reason),
            MailError::Transient(reason) => NotifyError::Transient(reason),
        })
    }
}

fn format_time(instant: OffsetDateTime) -> String {
    let format = format_description!("[year]-[month]-[day] [hour]:[minute] UTC");
    instant.to_offset(time::UtcOffset::UTC).format(&format).unwrap_or_default()
}

fn money(amount: Decimal) -> String {
    let mut rounded = amount.round_dp_with_strategy(MONEY_SCALE, RoundingStrategy::MidpointAwayFromZero);
    rounded.rescale(MONEY_SCALE);
    rounded.to_string()
}

// Every interpolated value is escaped for its HTML context, so a customer named "<script>..." cannot inject markup.
fn render(event: &OrderStatusChanged) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<body style=\"font-family: sans-serif\">\n  <p>Hello, {name}.</p>\n  \
         <p>Your order <strong>{order}</strong> changed from <strong>{old}</strong> to <strong>{new}</strong>.</p>\n  \
         <p>Total: {total}</p>\n  <p>Updated at {time}.</p>\n</body>\n</html>",
        name = escape(&event.customer_name),
        order = event.order_id,
        old = event.old_status,
        new = event.new_status,
        total = money(event.total_amount),
        time = format_time(event.changed_at),
    )
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use uuid::Uuid;

    use super::*;
    use crate::domain::OrderStatus;

    #[derive(Clone, Default)]
    struct Outbox {
        sent: Arc<Mutex<Vec<(String, String, String)>>>,
        failure: Arc<Mutex<Option<MailError>>>,
    }

    #[async_trait]
    impl Mailer for Outbox {
        async fn send(&self, to: &str, subject: &str, html_body: &str) -> Result<(), MailError> {
            if let Some(failure) = self.failure.lock().unwrap().clone() {
                return Err(failure);
            }
            self.sent.lock().unwrap().push((to.into(), subject.into(), html_body.into()));
            Ok(())
        }
    }

    fn event(name: &str, email: &str) -> OrderStatusChanged {
        OrderStatusChanged {
            order_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
            customer_email: email.into(),
            customer_name: name.into(),
            old_status: OrderStatus::Open,
            new_status: OrderStatus::Confirmed,
            total_amount: Decimal::from_str_exact("40.275").unwrap(),
            changed_at: OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        }
    }

    #[tokio::test]
    async fn it_sends_the_status_and_the_details_to_the_customer() {
        let outbox = Outbox::default();
        EmailNotifier::new(outbox.clone()).notify(&event("Ada", "ada@example.com")).await.unwrap();
        let sent = outbox.sent.lock().unwrap();
        let (to, subject, body) = &sent[0];
        assert_eq!((to.as_str(), subject.as_str()), ("ada@example.com", "Order CONFIRMED"));
        for expected in [
            "Hello, Ada.",
            "123e4567-e89b-42d3-a456-426614174000",
            "from <strong>OPEN</strong> to <strong>CONFIRMED</strong>",
            "Total: 40.28",
            "Updated at 2027-01-15 08:00 UTC.",
        ] {
            assert!(body.contains(expected), "{expected}\n{body}");
        }
    }

    #[tokio::test]
    async fn the_customer_name_cannot_inject_markup() {
        let outbox = Outbox::default();
        let hostile = "<script>alert('x')</script> & \"quotes\"";
        EmailNotifier::new(outbox.clone()).notify(&event(hostile, "ada@example.com")).await.unwrap();
        let body = outbox.sent.lock().unwrap()[0].2.clone();
        assert!(!body.contains("<script>"));
        assert!(body.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;quotes&quot;"));
    }

    #[tokio::test]
    async fn an_invalid_recipient_is_permanent_and_nothing_is_sent() {
        let outbox = Outbox::default();
        for address in ["", "not an email", "a@b.co\r\nBcc: x@y.z", "Ada <ada@example.com>"] {
            let result = EmailNotifier::new(outbox.clone()).notify(&event("Ada", address)).await;
            assert_eq!(result, Err(NotifyError::Permanent("invalid recipient".into())), "{address:?}");
        }
        assert!(outbox.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_mailer_failures_keep_their_kind() {
        let outbox = Outbox::default();
        let notifier = EmailNotifier::new(outbox.clone());
        *outbox.failure.lock().unwrap() = Some(MailError::Transient("timeout".into()));
        assert_eq!(notifier.notify(&event("Ada", "a@b.co")).await, Err(NotifyError::Transient("timeout".into())));
        *outbox.failure.lock().unwrap() = Some(MailError::Permanent("invalid header".into()));
        assert_eq!(
            notifier.notify(&event("Ada", "a@b.co")).await,
            Err(NotifyError::Permanent("invalid header".into()))
        );
    }

    #[test]
    fn money_has_two_places_rounded_half_up() {
        assert_eq!(money(Decimal::from_str_exact("0.005").unwrap()), "0.01");
        assert_eq!(money(Decimal::from_str_exact("10").unwrap()), "10.00");
    }
}
