//! The mailer against an SMTP server that speaks the protocol (a small one, in this process), so no external service is
//! needed: what is checked is what goes on the wire.

use std::sync::{Arc, Mutex};

use orderapi::domain::{OrderStatus, OrderStatusChanged};
use orderapi::messaging::{EmailNotifier, MailError, Mailer, Notifier, NotifyError, SmtpMailer, SmtpSettings};
use rust_decimal::Decimal;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
struct Received {
    mail_from: String,
    rcpt_to: Vec<String>,
    data: String,
}

/// An SMTP server that accepts mail and records it. It never offers STARTTLS.
async fn smtp_server() -> (u16, Arc<Mutex<Vec<Received>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let inbox = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&inbox);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                let mut mail = Received::default();
                write.write_all(b"220 localhost ESMTP test\r\n").await.ok();
                while let Ok(Some(line)) = lines.next_line().await {
                    let upper = line.to_uppercase();
                    if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                        write.write_all(b"250-localhost\r\n250 8BITMIME\r\n").await.ok();
                    } else if upper.starts_with("MAIL FROM") {
                        mail.mail_from = line["MAIL FROM:".len()..].trim().to_owned();
                        write.write_all(b"250 OK\r\n").await.ok();
                    } else if upper.starts_with("RCPT TO") {
                        mail.rcpt_to.push(line["RCPT TO:".len()..].trim().to_owned());
                        write.write_all(b"250 OK\r\n").await.ok();
                    } else if upper == "DATA" {
                        write.write_all(b"354 go ahead\r\n").await.ok();
                        while let Ok(Some(data_line)) = lines.next_line().await {
                            if data_line == "." {
                                break;
                            }
                            mail.data.push_str(&data_line);
                            mail.data.push('\n');
                        }
                        recorded.lock().unwrap().push(std::mem::take(&mut mail));
                        write.write_all(b"250 queued\r\n").await.ok();
                    } else if upper == "QUIT" {
                        write.write_all(b"221 bye\r\n").await.ok();
                        break;
                    } else {
                        write.write_all(b"250 OK\r\n").await.ok();
                    }
                }
            });
        }
    });
    (port, inbox)
}

fn settings(port: u16, require_tls: bool) -> SmtpSettings {
    SmtpSettings {
        host: "127.0.0.1".into(),
        port,
        user: None,
        password: None,
        from: "orders@example.com".into(),
        require_tls,
    }
}

#[tokio::test]
async fn a_mail_is_delivered_with_the_sender_the_recipient_the_subject_and_the_html_body() {
    let (port, inbox) = smtp_server().await;
    let mailer = SmtpMailer::new(&settings(port, false)).unwrap();

    mailer.send("ada@example.com", "Order CONFIRMED", "<p>hello</p>").await.unwrap();

    let inbox = inbox.lock().unwrap();
    assert_eq!(inbox.len(), 1);
    let mail = &inbox[0];
    assert!(mail.mail_from.contains("orders@example.com"), "{mail:?}");
    assert_eq!(mail.rcpt_to.len(), 1);
    assert!(mail.rcpt_to[0].contains("ada@example.com"));
    assert!(mail.data.contains("Subject: Order CONFIRMED"), "{}", mail.data);
    assert!(mail.data.to_lowercase().contains("content-type: text/html"), "{}", mail.data);
    assert!(mail.data.contains("<p>hello</p>"), "{}", mail.data);
}

#[tokio::test]
async fn the_notifier_and_the_mailer_work_together_on_a_real_exchange() {
    let (port, inbox) = smtp_server().await;
    let notifier = EmailNotifier::new(SmtpMailer::new(&settings(port, false)).unwrap());
    let event = OrderStatusChanged {
        order_id: Uuid::new_v4(),
        customer_email: "ada@example.com".into(),
        customer_name: "Ada".into(),
        old_status: OrderStatus::Open,
        new_status: OrderStatus::Canceled,
        total_amount: Decimal::new(1999, 2),
        changed_at: OffsetDateTime::now_utc(),
    };

    notifier.notify(&event).await.unwrap();

    let inbox = inbox.lock().unwrap();
    assert!(inbox[0].data.contains("Subject: Order CANCELED"), "{}", inbox[0].data);
    assert!(inbox[0].data.contains("19.99"), "{}", inbox[0].data);
}

#[tokio::test]
async fn when_tls_is_required_and_the_server_does_not_offer_starttls_nothing_is_sent() {
    let (port, inbox) = smtp_server().await;
    let mailer = SmtpMailer::new(&settings(port, true)).unwrap();

    let result = mailer.send("ada@example.com", "Order CONFIRMED", "<p>x</p>").await;

    assert!(matches!(result, Err(MailError::Transient(_))), "{result:?}");
    assert!(inbox.lock().unwrap().is_empty(), "the message must not go out in clear text");
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_transient_failure_not_a_permanent_one() {
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mailer = SmtpMailer::new(&settings(closed, false)).unwrap();
    let result = mailer.send("ada@example.com", "Order CONFIRMED", "<p>x</p>").await;
    assert!(matches!(result, Err(MailError::Transient(_))), "{result:?}");

    let notifier = EmailNotifier::new(mailer);
    let event = OrderStatusChanged {
        order_id: Uuid::new_v4(),
        customer_email: "ada@example.com".into(),
        customer_name: "Ada".into(),
        old_status: OrderStatus::Open,
        new_status: OrderStatus::Confirmed,
        total_amount: Decimal::ONE,
        changed_at: OffsetDateTime::now_utc(),
    };
    assert!(matches!(notifier.notify(&event).await, Err(NotifyError::Transient(_))));
}
