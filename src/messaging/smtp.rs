use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, MessageBuilder};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::email::{MailError, Mailer};
use crate::domain::is_valid_email;

const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The connection to the mail server. The password never appears in `Debug`.
#[derive(Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub from: String,
    /// Makes STARTTLS mandatory: if the server does not offer it the mail is not sent, instead of going out (and the
    /// credentials with it) in clear text. Production sets it.
    pub require_tls: bool,
}

impl fmt::Debug for SmtpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SmtpSettings(host={}, port={}, from={}, require_tls={})",
            self.host, self.port, self.from, self.require_tls
        )
    }
}

/// The settings are not usable.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SmtpConfigError(pub &'static str);

/// Sends mail through an SMTP server with lettre.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

opaque_debug!(SmtpMailer);

impl SmtpMailer {
    pub fn new(settings: &SmtpSettings) -> Result<Self, SmtpConfigError> {
        if settings.host.trim().is_empty() || settings.port == 0 {
            return Err(SmtpConfigError("smtp host and a valid port are required"));
        }
        if !is_valid_email(&settings.from) {
            return Err(SmtpConfigError("smtp sender must be a bare e-mail address"));
        }
        let from = settings
            .from
            .parse::<Mailbox>()
            .map_err(|_| SmtpConfigError("smtp sender must be a bare e-mail address"))?;
        let tls = TlsParameters::new(settings.host.clone())
            .map_err(|_| SmtpConfigError("the TLS settings are not usable"))?;
        let builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host)
            .port(settings.port)
            .timeout(Some(SMTP_TIMEOUT))
            // STARTTLS when the server offers it; refuse to go on without it when it is required.
            .tls(if settings.require_tls { Tls::Required(tls) } else { Tls::Opportunistic(tls) });
        let builder = match settings.user.as_deref().filter(|user| !user.is_empty()) {
            Some(user) => {
                builder.credentials(Credentials::new(user.to_owned(), settings.password.clone().unwrap_or_default()))
            }
            None => builder,
        };
        Ok(Self { transport: builder.build(), from })
    }

    fn message(&self, to: &str, subject: &str, html_body: &str) -> Result<Message, MailError> {
        // Header values with CR or LF are refused: they are how a header injection adds a Bcc.
        if [to, subject].iter().any(|value| value.contains(['\r', '\n'])) {
            return Err(MailError::Permanent("invalid header".into()));
        }
        let recipient = to.parse::<Mailbox>().map_err(|_| MailError::Permanent("invalid recipient".into()))?;
        MessageBuilder::new()
            .from(self.from.clone())
            .to(recipient)
            .subject(subject)
            .header(ContentType::TEXT_HTML)
            .body(html_body.to_owned())
            .map_err(|_| MailError::Permanent("the message could not be built".into()))
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, to: &str, subject: &str, html_body: &str) -> Result<(), MailError> {
        let message = self.message(to, subject, html_body)?;
        // The server's answer can quote the address; the log gets the kind of failure, not the text.
        self.transport.send(message).await.map(|_| ()).map_err(|failure| {
            MailError::Transient(
                if failure.is_permanent() {
                    "the server refused the message"
                } else {
                    "the server could not be reached"
                }
                .into(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> SmtpSettings {
        SmtpSettings {
            host: "mail.example.com".into(),
            port: 587,
            user: Some("mailer".into()),
            password: Some("hunter2".into()),
            from: "orders@example.com".into(),
            require_tls: true,
        }
    }

    fn mailer() -> SmtpMailer {
        SmtpMailer::new(&settings()).unwrap()
    }

    #[test]
    fn the_settings_are_validated() {
        assert!(SmtpMailer::new(&settings()).is_ok());
        for bad in [
            SmtpSettings { host: " ".into(), ..settings() },
            SmtpSettings { port: 0, ..settings() },
            SmtpSettings { from: "Orders <orders@example.com>".into(), ..settings() },
            SmtpSettings { from: "nobody".into(), ..settings() },
        ] {
            assert!(SmtpMailer::new(&bad).is_err());
        }
    }

    #[test]
    fn the_password_never_appears_when_the_settings_are_printed() {
        let text = format!("{:?}", settings());
        assert!(!text.contains("hunter2") && !text.contains("mailer"), "{text}");
        assert!(text.contains("mail.example.com") && text.contains("require_tls=true"));
    }

    #[test]
    fn a_header_value_with_a_line_break_is_refused_as_an_injection() {
        for (to, subject) in [("a@b.co\r\nBcc: x@y.z", "Order"), ("a@b.co", "Order\nBcc: x@y.z"), ("a@b.co\n", "Order")]
        {
            assert_eq!(
                mailer().message(to, subject, "<p>x</p>").unwrap_err(),
                MailError::Permanent("invalid header".into())
            );
        }
    }

    #[test]
    fn a_recipient_that_is_not_an_address_is_permanent() {
        assert_eq!(
            mailer().message("not an address", "Order", "x").unwrap_err(),
            MailError::Permanent("invalid recipient".into())
        );
    }

    #[test]
    fn a_well_formed_message_is_html_from_the_configured_sender() {
        let message = mailer().message("ada@example.com", "Order CONFIRMED", "<p>hi</p>").unwrap();
        let text = String::from_utf8(message.formatted()).unwrap();
        assert!(text.contains("From: orders@example.com") && text.contains("To: ada@example.com"), "{text}");
        assert!(text.contains("Subject: Order CONFIRMED") && text.to_lowercase().contains("text/html"), "{text}");
    }
}
